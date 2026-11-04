# Keeping brokkr and its projects in step across versions

Design discussion, begun 2026-10-30. It records the problem, what
triggered it, and the chosen direction (2026-11-04): benches are Lua
scripts in brokkr.toml. The alternatives weighed on the way (version gates,
a self-describing binary, a migration chain, declarative definitions read
at the commit under test) are in this file's git history.

## The problem

brokkr drives each project's binary through a contract it does not own: the
command line (commands, flags, values, defaults), the exit codes and what
they mean, the outputs a command writes (files, stdout, formats), side
channels (the marker FIFO, `BROKKR_*` variables) and, for some projects,
harness protocols (pbfhogg's library harness, piners' corpus harness).

The projects change that contract over time. brokkr has to keep working
against every version of it that a `--commit` baseline, an A/B or a verify
run can land on, not only against HEAD.

Today brokkr encodes each project's contract by hand, as of whenever someone
last edited the typed command layer (`src/pbfhogg/commands.rs` and its
counterparts). It is correct for roughly one point in history, and drift is
found by runs failing.

## What triggered it

One session's worth of pbfhogg 0.6.0 tickets, every one of them contract
drift:

- **bz 67**: pbfhogg 0.6.0 gave every short flag one meaning across all
  commands and refused the retired letters with exit 64. brokkr still sent
  `tags-filter -i` and `-t`. Fixed by spelling every flag long, except
  verify renumber's `diff -s`, whose long form was renamed (`--summary` to
  `--osmium-summary`) while `-s` kept its meaning, so `-s` is the only
  spelling every baseline with the option accepts.
- **bz 64**: pbfhogg added `check --index`; brokkr had no way to measure it.
- **bz 68**: text diff output outgrew the 16 MiB stdout capture at planet
  scale; brokkr now passes `--output`, which changes what older rows
  measured without anything in the row saying so.
- **bz 65**: pbfhogg 0.6.0 split its exit codes: 1 is a negative answer from
  a completed run, 2 a failure, 3 a host I/O feature unavailable, 64 a usage
  error. Before 0.6.0, exit 1 meant both "no" and "crashed". brokkr accepts
  exit 1 as a completed answer for `diff`, `diff-snapshots`, `check-refs`,
  `check-ids` and `check-index` (`ok_exit_codes`), for every build. A
  `--commit` baseline at v0.5.0 or earlier that crashed with exit 1 is
  therefore stored as a clean answer. The pbfhogg dev was about to run
  v0.5.0 baselines when this was raised.
- The `-j` help text claimed pbfhogg defaults that were never or no longer
  true (`inspect` "1", `apply-changes` "nproc-2"); pbfhogg 0.6.0 has one
  `-j` meaning everywhere, default `0` = sized automatically.

A later one, the same shape:

- **bz 73**: pbfhogg 587bfc12 made the text diff one streaming engine that
  writes no scratch and refuses `--tmp-dir` with exit 64. brokkr passed
  `--tmp-dir` to every text diff, so verify diff, derive-changes, renumber
  and the diff bench failed at startup. Fixed against HEAD only: brokkr no
  longer declares scratch for a text diff, so a baseline from before
  587bfc12 now drops its text-diff scratch wherever pbfhogg defaults it,
  outside the per-launch private directory. A script branching on
  `since("587bfc12")` keeps both sides right.

## Benches are scripts in brokkr.toml (2026-11-04)

brokkr builds the binary under test from the bench's build settings
(below); each bench is a `[bench.NAME]` table, inline or through `include`,
holding a Lua script that runs it:

```toml
[bench.cat]
script = """
function bench(params)
    return binary("cat", params.dataset)
end"""
```

(Params reach the script as one table, never as globals, so no param can
shadow a builtin: `bench(params)`'s argument, settled in slice 5.)

The script is inline in `script`, as above, or in a `.lua` file named by
`script_file` (`script_file = "benches/cat.lua"`), resolved relative to the
real directory of the TOML file that set that field, as `include` paths
are; composition tracks provenance per field, not per table, since a
bench's script and build settings may come from different files. An entry
gives one or the other; both in one file is a load error, and a higher
layer giving either replaces whichever a lower one gave (landed in slice
2, `docs/brokkr.toml.bench.md`).

The top-level names `bench`, `bench_params` and `data` are reserved as
sections (approved by the user, 2026-11-04): only a host literally so
named would change meaning, and it moves to `[host.NAME]`.

- **HEAD's scripts serve every commit.** brokkr reads benches only from the
  working tree's `brokkr.toml`, never at the commit under test. HEAD knows
  all of history, so a script that must invoke an older binary differently
  branches on a builtin, `since("<sha>")`, true when the commit under test
  descends from that sha (`git merge-base --is-ancestor`):
  ```lua
  if exit == 1 and since("854f9465") then return "differs" end
  ```
  The projects keep linear history on master, so before and after are one
  order. A sha `since` cannot resolve is an error, never a guess. Commits
  from before adoption need no fallback, backfilling history is an edit at
  HEAD, and a project prunes branches for baselines it no longer cares
  about.
- **The script is the logic.** Chaining (`apply-changes` into `diff`),
  conditional flags, scratch and exit meanings are code, not vocabulary;
  argv templates, role selectors, derived inputs and answers tables are not
  needed.
- **The whole block is measured.** A bench is what its script does; setup
  that should not be timed belongs in a dataset snapshot or another bench.
  brokkr's own surfaces work as today: `binary()` wires the marker FIFO,
  the sidecar and the stdout cap into every child, and `capture_env` is
  recorded once per run.
- **The name names the bench; pairing adds its inputs.** Rows pair on the
  name, the bound params and their resolved keys and digests (below). A
  script that changes what a bench measures under the same name is the
  project's concern, not brokkr's.
- **Globals come from the project's config**, so their names and shapes are
  the project's and move with its scripts. brokkr's own surface is the few
  functions it provides, such as `binary` and `since`.
- **The build is TOML, through `brokkr build`'s resolution** (decided for
  now, superseding a Lua `build()`). A bench table names `target`
  (optional, `[bin] default` otherwise), `profile` and `features`, the
  vocabulary of `brokkr run`, with `[bin]`, host `features`, `[cargo_env]`
  and rustflags folding in as today. Targets resolve by name through cargo
  metadata, at the commit's worktree under `--commit`, so pbfhogg's CLI
  crate split (ed5f0365) needs nothing: the bin is `pbfhogg` on both sides.
  `binary()` runs the content-keyed artifact `brokkr build` makes
  (`.brokkr/artifacts/<xxh128>/<name>`), whose digest the row records.
  Ancestry-keyed overrides are added only for what discovery cannot see, a
  renamed bin target or a profile an older commit lacks (ratatoskr's
  `harness`), when a real case needs one. `brokkr run` deliberately keeps
  no feature default in config, so the invocation shows what was built; a
  bench's `features` reverse that, made good by recording the resolved
  features on the row.
- **Instruments and I/O modes are the project's.** Hotpath, alloc,
  `--direct-io` and `--io-uring` are features and flags a project's benches
  set, not brokkr modes; brokkr keeps no per-mode feature table, host
  preflight or hotpath report parser for them. A sample is the block's
  wall and what the script returns.
- **What brokkr hands the script:** per-iteration scratch and output paths
  it reserves and cleans, and the resolved brokkr.toml (see datasets,
  below). A script decides whether to pass scratch on (bz 73,
  merge-changes).
- **`binary()` returns a result table, not throwing on a nonzero exit;
  `bench()` returns the verdict.** Which exits are answers is the script's
  call (bz 65); which outcomes reach the script at all is under failure,
  below. Declared answers (`expect`) are not ported: no config uses them,
  and a script that wants one reads it from the resolved config.
- **Without `--commit`, `since()` asks about the checkout's HEAD,** so an
  older commit checked out normally gets the older contract. Ancestry
  describes committed history only: while a CLI change and its script edit
  are both uncommitted, no sha names the new contract, and the script can
  only call it unconditionally until the commit lands. The sha is always
  resolved first, so a mistyped sha is an error.
- **Every child goes through brokkr:** no `os.execute`, `io.popen` or module
  loading in the Lua state; the script runs inside the lock hold, and a
  runaway script answers Ctrl-C once dellingr's interrupt is linked (slice
  13; until then pure Lua is stopped only at its next builtin call).
- **The typed layer stays.** Script benches are added beside the existing
  commands; verify, the suite, `--as-snapshot` promotion and the library
  harness keep using `src/pbfhogg/commands.rs` unchanged.
- **The verb is `brokkr bench NAME`;** criterion's wrapper is now
  `brokkr crit` (slice 1, landed), with no `bench` alias in between, its
  store still `.brokkr/bench/` so recorded baselines keep reading. The docs
  naming `brokkr bench` in protohoggr, pbfhogg, nidhogg and piners follow
  in those projects (protohoggr#2, pbfhogg#46, nidhogg#2, piners#66).
- **`since()` sees the project's own repo only.** Sibling path dependencies
  (nidhogg on pbfhogg and elivagar) are linked to the live checkout under
  `--commit`, so a row should record each sibling's commit and dirty state;
  that is a fix of its own, independent of script benches.
- **brokkr check loads every bench and compiles its Lua** without running
  anything, which catches syntax errors only: a misspelt builtin compiles.
  Running benches on a fixture is the project's option, as a
  `[[script_check]]`.
- **The engine is dellingr,** the user's pure Rust Lua VM, not vendored
  `mlua` (decided). dellingr gains an interrupt so Ctrl-C reaches a
  runaway script (dellingr#9); scripts are dellingr Lua, not Lua 5.4: no
  `pcall`, long strings, bitwise operators or `goto`, so an error always
  ends the sample, as script benches want anyway. Identity is taken from
  the bench's name and inputs, not an evaluated structure, so the script
  needs no hermetic, deterministic evaluation.

### Params and datasets (decided)

- **Bench parameters and the CLI.** `brokkr bench cat --jobs 4`: brokkr
  cannot know a bench's flags ahead of time. clap stays for brokkr's
  built-in commands; a bench's arguments are parsed apart from it, since
  they are known only once brokkr.toml loads. Params are declared in the
  bench's table (`params.jobs = { help = "..." }`), parsed by hand over the
  tokens clap passes through (slice 6: `--NAME VALUE` or `--NAME=VALUE`,
  help only where a name stands; four rules did not earn a lexer crate),
  handed to the script as a table and stored on the row, which gives real
  `--help`, typos refused before a build, and structured pairing.
- **brokkr's options go before the bench name, the bench's params after
  it** (slice 2, sparred): `brokkr bench --commit X cat --jobs 4`. Mixed on
  one line, every option a later slice adds to the verb would become a
  reserved param name and turn a loading config into an error; split, the
  only reserved param name is `help`, refused at load since slice 2. After
  NAME, clap consumes nothing, help included. Bench names are not reserved
  against brokkr's commands: under `bench NAME` they cannot collide.
- **Datasets stay brokkr's.** Downloads, digests, `brokkr env` status and
  snapshot registration are shared by pbfhogg, elivagar and nidhogg, so the
  dataset tables and their resolver stay in brokkr. A script reaches them
  through one builtin, `config()` (slice 6), that returns the whole
  resolved brokkr.toml as a table (includes folded, the host's sections
  selected, dataset paths absolute; `docs/brokkr.toml.bench.md`, The
  resolved view); the script picks its inputs.
- **A param selects into that config by a `from` path.** Each param names
  a table path whose keys are its valid values, and a path may name
  another param, resolved in dependency order; the script receives the
  entry at the chosen key.

  ```toml
  params.dataset = { from = "datasets", default = "denmark" }
  params.variant = { from = "datasets.{dataset}.pbf", default = "indexed" }
  params.osc     = { from = "datasets.{dataset}.osc" }
  ```

  Nothing in it knows datasets (mogwai targets or elivagar ocean sets work
  the same); an unknown key is refused before the build with the valid
  keys; `--help` can list them; the row records each param's chosen key and
  the entry's digest.
- **Snapshots** are another layer (`datasets.X.snapshot.Y.pbf.Z`) and the
  common case in pbfhogg's config. The resolved view exposes each
  dataset's primary as `snapshot.base` (its `pbf`, `osc` and
  `download_date`; no `seq`, which the primary has only per entry), so
  `from = "datasets.{dataset}.snapshot.{snapshot}.pbf"` with `snapshot`
  defaulting to `base` covers primary and snapshot with one path; a bench
  whose shape fits no single path walks the config in its script. The raw
  TOML does not change.

### Also decided

- **A row pairs on** the bench name, the bound params and each param's
  resolved key and digest; anything a script computes from neither does
  not pair. A re-download under the same key splits rows by its digest.
- **`_G.record`** is an empty table each sample; whatever the script puts
  in it (serializable values only, anything else an error at the end of the
  sample) is stored on the row, and takes no part in pairing. It is also
  how instrumentation is reported: `--hotpath` and `--alloc` die (the typed
  commands keep them until they are retired), and a bench that profiles
  names the feature in its build settings, points the report through
  `binary()`'s `env` table, reads it back and puts what it wants in
  `record`. brokkr keeps no report parser.
- **Script bench rows go to a new database,** per user rather than in the
  project's tree (today's results.db lives in the project's `.brokkr/`),
  local by default or on a results server; see the results store below.
  Rows carry the project they belong to; typed-command history stays in
  results.db. The sidecar's samples and markers are absorbed into the same
  database rather than a separate sidecar.db.
- **The user-wide include layer cannot define benches;** `[bench.*]` there
  is refused at load.
- **Calling the binary,** with ideas from the user's luash fork (folknor,
  version 4): a callable command object for the built binary, flags as a
  table (`true` a bare flag, `false` or `nil` dropped, so an unset param
  drops out), and a result table. Unlike luash, no shell: arguments become
  an argv array and every child goes through brokkr's spawn. Details under
  call style, below.
- **Pipelines run eagerly in v1:** one call's result fed to another passes
  through a brokkr scratch file. Streaming pipes (`cat | inspect -`) and a
  server with a client wait until a bench needs them; both arrive with one
  primitive, a non-waiting `spawn` whose stdout another call can take as
  stdin, waited on at the block's end. A server adds a readiness wait (a
  stderr line, an HTTP health check or a readiness file) and is killed when
  the block ends, even on failure. Per-child data already covers what
  either measures.
- **Implementation details:** error line numbers mapped back into an
  inline TOML script; Ctrl-C reaching pure Lua and top-level code as well
  as children (slice 13). The Lua state is per iteration (see the third review).

### Second review (2026-11-04)

A second codex review found no settled choice to reopen; its gaps were
settled as follows.

- **Failure.** A child's exit code and signal reach the script; Ctrl-C, a
  deadline, a spawn or wait failure, truncated output and a Lua error abort
  the sample before it sees them. `bench()` returning nothing completes the
  sample, a string completes it with that verdict, and `fail("reason")` or
  `error()` fails it. A result's `:ok()` raises unless the exit is 0, which
  is most benches' whole failure handling.
- **Container params pair on their key only;** a param resolving to a file
  pairs on key and digest. The whole resolved brokkr.toml is stored per run
  as provenance, once per distinct hash, so `--compare` can name a config
  change (a `bbox` or `getid_ids` edit) between paired rows without
  splitting them.
- **OSC ranges:** a `from` param may be a collection, with `a..b` expanded
  and each key checked, as `--osc-range` does today; every resolved file
  pairs like a single one (ordering and fan-out in the third review).
- **Several children per sample:** samples and markers are kept per child,
  in order, on one timeline relative to the block's start.
- **Scratch and outputs build on the existing machinery** (launch
  reservations, private directories, cleanup and retention); `output()`
  and `scratch()` are handles onto it. The third review extends the lease
  protocol and drops promotion.
- **File builtins** in the sandbox: `read`, `write`, `json.encode` and
  `json.decode`, `filesize`, and `print` routed through brokkr's output.
- **The build** was first settled as a Lua `build()` returning
  `BuildConfig`'s fields, then replaced by the TOML build above, which also
  removes the duplication across a project's benches.
- **Project data lives under `[data.*]`** (decided while porting
  dellingr): a top-level namespace brokkr reserves and never interprets,
  which `from` paths walk. An unknown top-level table is a host section, so
  project tables need it. dellingr keeps `[dellingr.workloads]` for
  `brokkr dellingr` and adds one file entry per scale for `brokkr bench`,
  `[data.workloads.NAME.bench]` and `.hotpath`, each `file` and `xxhash`,
  so a `scale` param resolves to a file and pairs on key and digest, and a
  re-registered workload cuts history as today.
- **A resolved file with a digest is verified before the build;** a
  mismatch is refused, naming the drifted pin, as `brokkr dellingr` does
  today. A file without one is hashed and recorded.
- **`binary()` runs the child in the live project root,** under `--commit`
  too, as `brokkr run --commit` does, so relative in-tree inputs keep
  coming from the working tree (dellingr's split-tree rule). A `cwd`
  option waits for a bench that needs the worktree.
- **A param given several values fans out:** `--workload a,b,c`, or `all`
  for every key under its `from` table, makes one build and one run per
  value, each its own row; a failure does not stop the rest and the failures
  are listed at the end; several fanned params give the cross product.
- **Stderr `key=value` scraping moves to Lua:** brokkr scrapes nothing
  from a script bench's children; a script parses what it wants into
  `record`.
- **A named hotpath table in `record`** (`record.hotpath` holding a hotpath
  report) is rendered by `brokkr res` with today's pretty printer and
  `--top N`; display only, nothing is captured or parsed during a run.
- **External tools** (dellingr's reference interpreters, pbfhogg's osmium)
  wait for a bench that needs them, through a `tool(name)` command object
  whose path and version would go on the row. A cross-arm check on
  dellingr's cost fingerprints is not added: nothing checks them today.
- **Shared parameter sets:** `[bench_params.NAME]` holds params a bench
  pulls in with `uses = ["NAME"]` (pbfhogg's dataset, snapshot and variant
  trio); a bench's own `params` override a set's of the same name.
- **`from` syntax:** the string form; `{x}` inserts param x's chosen key,
  never its table; params resolve in dependency order and a cycle is
  refused at load; a key containing a dot is refused where it is
  substituted into a `from` path, at run time (slice 6: the stored path is
  segments joined by dots; a key selected at a path's end may hold one);
  a dependent param's
  `--help` lists the values under its parents' defaults; a param without
  `from` is a plain string.
- **Call style:** as in the user's luash fork, positional arguments in call
  order and a flag table emitted sorted by key, so the recorded argv is
  stable; `true` a bare flag, `false` or `nil` dropped, a list repeating the
  flag; a value passed as its own argv entry (`--jobs 4`, never
  `--jobs=4`), which dellingr's harness requires. The result carries exit
  code, signal, and stdout and stderr previews over full spools (third
  review); `env` adds to the child's environment; feeding one result to
  another uses the spool.
- **The global database:** rows are keyed by project name, the checkout
  recorded as data, since clones share history. `brokkr res` reads it for
  now. What a run stores is in the third review's evidence model.

### Third review (2026-11-04)

A codex spar over the plan, to consensus. It corrected the decisions
above where they disagree; this section wins.

- **Attempted runs.** Every attempted run gets an identity and a persisted
  outcome, with its iterations and their children beneath it; the
  benchmark result is optional. Only runs with a result pair or compare.
  Any iteration failing fails the run, as today; completed iterations
  whose verdicts disagree fail it too (nil and strings compare exactly,
  any other return type fails the sample). A successful run's result is
  the fastest iteration, with every iteration's wall in execution order.
- **Evidence.** Everything the sidecar records today (samples, markers,
  counters, tagged records, sink ends, capture information, clock
  diagnostics) is kept per child for every iteration, failed ones
  included, with child-local timestamps plus each child's origin offset
  from the block's start, not rebased.
- **Lua state per iteration.** A fresh state each iteration, so globals do
  not carry work between them. No resource I/O or child execution before
  `bench()`: `read`, `write`, `filesize`, `binary`, `output` and `scratch`
  raise at top level; pure computation, `json` and the resolved
  config are allowed, since all hashing and digest verification happen in
  preflight. Timing starts when `bench()` is called; the deadline and the
  interrupt cover state initialization and top-level code too. This was
  ruled a prerequisite of the runner; the user then ruled (2026-11-04) that
  the runner lands against dellingr 0.6.0 without it, children being
  interruptible already, and the interrupt and the deadline follow as slice
  13.
- **Handles.** Paths reach a child as handles: bound file-valued params are
  input handles, `output()` and `scratch()` make output handles. (Slice 6
  narrowed "the resolved config's file entries" to bound params, sparred:
  making every file entry an input would hash and reserve every dataset
  file, planet included, before each run and refuse a bench over one
  missing unrelated file; a file a script picks from `config()` is a path
  string, undeclared, and pairs on nothing, as anything a script computes
  from neither params nor digests does not.) Handles guarantee that declared
  resources are reserved; they do not confine a child, and a literal
  string the child treats as a path is undeclared. A handle's origin
  gives its default access per `binary()` call (input read, output and
  scratch write; write implies read); a call's `reads` and `writes` lists
  override it and also declare paths embedded where brokkr cannot see a
  handle (a JSON or config file the script wrote, a `--output=PATH`
  spelling); a handle in both lists is refused. `read()` takes any
  handle, `write()` output and scratch handles only.
- **Inputs are read-only for the run.** A declared write overlapping a
  preflight-hashed input is refused at the call, so its preflight digest
  stays current; and the run holds a read reservation on every input from
  before its hash to the run's end (slice 6), so no other lease writes it
  between the digest and the last iteration
  (`reference/bench-resources.md`, The run's inputs).
- **Ownership.** Outputs, scratch and stdout spools belong to the
  iteration, which holds an owner lease; each child launches with a borrow
  the owner authorizes, a sub-reservation that does not conflict with its
  owner. A child finishing releases its borrow only; the owner releasing,
  when the run publishes or fails, makes the resources reclaimable. Crash
  reclamation uses the existing lease and private-directory sweep with the
  iteration as owner. This extends the launch-lease protocol, which today
  ties reclamation to the launch's own release.
- **Capture.** Every child's stdout is spooled in full to an
  iteration-owned file; `result.stdout` is a bounded preview with a
  truncated flag, and truncation no longer aborts the sample. Feeding a
  result onward uses the spool.
- **Collections and fan-out.** A param declared `many = true` is a
  collection: comma lists, ranges and repeats build one list bound to one
  sample, in command-line order with ranges expanded in place ascending,
  and `all` in key order; duplicates are refused, and the order is part of
  pairing. Numeric order applies only when every key is a canonical
  decimal (no sign, no leading zero), lexical otherwise, and range
  endpoints must be canonical. A collection param named in another
  param's `from` path is refused at load. Fan-out applies to
  non-collection params: dependents resolve per parent binding, and an
  explicitly given child key missing under any parent fails preflight for
  the whole invocation before any build; an arm failing at run time does
  not stop the rest.
- **No promotion (user, 2026-11-04).** Script benches cannot register an
  output as a dataset snapshot: making a derived input is setup, not
  measurement, and no planned bench needs it. The typed commands keep
  `--as-snapshot`, and a bench consumes the snapshots they make through
  `from` like any other.
- **Build features** fold host features in as measured contexts do today,
  which `brokkr build`'s resolver alone does not; the row records the
  requested and the cargo-resolved features, and a key controls default
  features.
- **`[data]` file entries** are `{ file, xxhash }`, the key name datasets
  use, `file` relative to the project root.
- **The server URL** is a user-only key in the user-wide config, refused in
  a project config.
- **The remote write contract** lands with the server, not after it: each
  request id is bound to its request's content, reuse with different
  content is refused, outcomes are retained for replay and stored in the
  same transaction as the write, and an error says whether the write is
  known unapplied or its outcome uncertain (a commit error can be
  either), as bz does.

### The results store (2026-11-04)

The user's rows live on their NAS, reached over Tailscale, modelled on
bz (`../bz`, its `reference/architecture.md`). bz is private and brokkr is
public with third-party users, which is where the two part.

- **Two backends, one store.** With nothing configured, the database is a
  local SQLite file under `$XDG_DATA_HOME/brokkr/`, so a user without a
  server loses nothing. A results server is opt-in, named by a URL in the
  user-wide config; brokkr compiles in no host, tailnet or port (bz does,
  being private). Both backends share one schema and one migration chain,
  so the server is the local store with a network layer in front.
- **The server owns the file.** As in bz: it holds one local SQLite file
  in WAL mode, refuses to start where WAL is unavailable or another server
  holds the file (an `flock` beside it), runs every write on one worker
  thread under `BEGIN IMMEDIATE`, and migrates at startup through a frozen
  `user_version` chain, refusing a database newer than itself. Clients
  never migrate. No SQLite file on a network mount.
- **No offline spool and no application auth (decided).** A configured
  server that cannot be reached fails the command, after retries within a
  bounded budget. Who can reach the server is the network's business; on
  the user's own setup Tailscale Serve terminates HTTPS and forwards to a
  loopback port.
- **Retried writes are idempotent.** Each write carries a client-generated
  request id, resent on every retry; the server stores the outcome under
  it in the same transaction, so a retry is answered from that record
  instead of applied twice, and a write that may have landed without an
  answer is reported as outcome unknown, naming the id. A proxy's 5xx is
  retried; the server's own errors say whether the write is known
  unapplied or uncertain (third review). For publishing a run the request
  id is the run's own uuid and the stored run is the outcome record
  (slice 3, `reference/bench-store.md`).
- **Its own binary.** The server is a separate binary in this crate,
  beside `brokkr-rustc-guard`, not a brokkr subcommand.
- **A maintained HTTP stack, not bz's.** bz's hand-written HTTP/1.1 subset
  replaced `tiny_http` (bz 0db8048), which drains an unread request body on
  whichever thread drops the request, with no deadline, so a stalled
  client froze the accept loop; bz is synchronous throughout, which ruled
  out the async servers. brokkr already links tokio through
  nextest-runner, so the server uses `hyper` or `axum` on it, with request
  and idle timeouts and a cap on requests in flight (bz answers 503 beyond
  64). The client stays blocking: ureq 3 with rustls, as bz uses (slice
  12, `src/bench/remote.rs`).
- **Deployment is documented, not automated:** a systemd user unit and a
  Tailscale Serve route as an example, and backups through `sqlite3
  .backup`. bz's hostname-gated `bz deploy` is specific to one host and is
  not carried over.

### Build order

Slices sized for one agent launch each, run serially; each leaves
`brokkr check` green and is committed before the next starts. Sparred
with codex to consensus (2026-11-04): the store and the resource protocol
come before the runner, so the runner does not decide them by accident,
and params come after it, exercised against a running bench. The
contracts the slices build against are the sections above; the third
review was slice 0. Landed: slice 1, the criterion wrapper's move from
`brokkr bench` to `brokkr crit`; slice 2, the config schema
(`docs/brokkr.toml.bench.md`): `[bench.*]`, `[bench_params.*]` with
`uses`, `[data]`, per-value provenance in `ConfigSources`, dellingr 0.6.0
from crates.io behind `src/bench/lua.rs`, and check's `bench` phase. Slice
2 also took every static load-time refusal of a param declaration (strict
fields, `from` template syntax, unknown `{param}` references, cycles, a
`many` param in a `from` path, judged on each bench's effective params),
so no later slice tightens a loading config; what needs the resolved
config went to slice 6 (run time) and, for collections, slice 7. Slice 3, the evidence model and the
local store (`reference/bench-store.md`): `$XDG_DATA_HOME/brokkr/bench.db`,
a self-contained `src/bench/store/` a server binary includes by path, one
validated `RunDocument` per attempted run published in one transaction
with the run uuid as request id (replay answered, different content
refused, commit errors reported as uncertain), the result and pairing key
derived by the store, a frozen migration chain pinned by digest, and
`src/bench/evidence.rs` converting a sidecar capture. `record` is kept per
iteration, the result's being its best iteration's. Slice 4, the resource
protocol (`reference/bench-resources.md`): an owner lease per iteration
that brokkr alone holds and a borrow per child naming it, the child
inheriting the borrow only; a held borrow pins its free owner, every lease
classified before any is reclaimed, an unreadable record keeping every
free owner; call admission by a read-only veto (inputs and earlier
spools), authority by current resolution and the ordinary conflict rule
against all but the owner; areas, scratch and spools made through held
directory descriptors that a displaced area fails; stdout spooled to a
file with a bounded preview; handles in `src/bench/resources.rs`. Sparred
against two rivals: the child inheriting the owner's descriptor too
(an owner-only close would let a crash reclaim the iteration under a
running child) and the library's serial coverage launches (no crash
reclamation without a name clean knows, no per-call completion). Slice 9,
taken before 5 (`docs/commands/results.md`, Script benches): `brokkr
results` itself reads the store (no `res` name), the current project's runs
after the typed output in every view; a read-only open that never creates
the file; `RunFilter` and the listing queries in the store module, in one
total order (start time, then uuid); filters mapped (`--command` to the
bench name, `--param` new, `--mode`/`--dataset`/`--meta` excluding bench
runs); `--compare` on completed clean runs, latest per stored key, naming
config paths, build, host, env, verdict and record moves, collapsed arms and
an equal-context pointer; `record.hotpath` through the typed printer. Sparred
against a selector flag and a separate verb.
Slice 11, taken before 5 (`reference/bench-store.md`, On the wire;
`reference/results-server.md`; `docs/results-server.md`): the wire lives in
the store (`src/bench/store/wire.rs`), one `POST /bench` with externally
tagged requests and answers, strict, one protocol version at a time with
frozen fingerprints and frozen decoding refusals; a failure envelope of its
own (kind, message, `applied` only when proved for that attempt, `retry`),
tolerant and stable across versions, from which a client rebuilds a
`StoreError` only when the envelope is exactly one; `brokkr-results-server`
as a third bin including the store by `#[path]`, hyper 1 on tokio, one
request per connection, admission before the body, a single worker with a
compare-and-swap between claim and withdrawal, the autocommit check, the
canonical-path flock. Sparred against `StoreError` itself as the envelope,
REST routes, a lib target, and tolerant versioned answers.

Slice 5, the runner core (`docs/commands/bench.md`, man topic `bench`):
`brokkr bench [--commit REF] [--iterations N] [NAME [PARAMS...]]`, clap
reading nothing after NAME; bare lists, `NAME --help` describes; a bench
declaring params is refused until slice 6. The script is read and compiled
once per run and every iteration runs it in a fresh State; builtins
`binary`, `output`, `scratch` (raising at top level), `since` (cached per
run, allowed at top level), `fail`, `record`, and `print` through the
console channel, all marshalled in `src/bench/lua.rs` behind a `Host` trait
with Lua strings crossing as bytes. The build is `brokkr build`'s target
resolution and snapshot with host features folded in, a new additive
`default_features` key, and cargo's resolved features read from the
compiler-artifact record. `binary()` runs on a borrow, stdout spooled,
stderr a bounded pipe (both previews 1 MiB, truncation failing nothing),
one marker FIFO per child drained while the borrow's release waits
(`Call::finish_observed`, the sidecar split into supervision and finish).
Iterations live in `.brokkr/bench-iterations/`, and every recursive removal
of a configured directory in any project now goes through an overlap-only
gate (`reference/bench-resources.md`, Placement and cleanup, with the
fixed-name and alias exclusions); `brokkr clean` reclaims leases in every
project. Dirty runs are stored and marked. The stored config is one TOML
document: `[run]` roots, `[script]` exact source and origin, `[config]` the
composed table sorted. Publication latches uncertainty across up to three
republishings, the uuid line is forced, and `results` opens in litehtml
with the typed reader skipped. Sparred against a Host-free seam, scratch
placement with enumerated gates, the composed table alone as provenance,
and reopening one FIFO per iteration; the spar added the per-child FIFO
with the pumped release, the record shape and depth checks, the byte-exact
seam, the latched uncertainty, the project root in the stored config and
the alias boundary.

What slice 5 left, with sizes:

- Its per-call overhead: in the first real run (`brokkr bench startup` in
  brokkr's own config, a 6 ms child) a `binary()` call started about 10 ms
  into the block and the block took about 22 ms; admission, spool, FIFO and
  the lease records' syncs are all inside the timed block by the design
  ("the whole block is measured"). Fine for real workloads, distorting for
  millisecond ones; slice 10 should judge it on a real bench before anyone
  optimizes it (small: profile the call path, likely the syncs).
- The iteration directory sits on the project's disk; a host-chosen
  location for benches writing large outputs (slice 10's pbfhogg writers)
  needs a key and the same gate obligation (small).
- A child's stderr is a 1 MiB preview only; a bench needing all of it wants
  a stderr spool, a protocol change in `reference/bench-resources.md`
  (small, slice 8 if a bench needs it).

Slice 12, the remote client, taken before 6 (`reference/bench-store.md`,
brokkr's client; `docs/brokkr.toml.md`, `results_server`;
`docs/results-server.md`): `results_server` in the user-wide config, taken
out of the user table before any merge (so never in the composed or stored
config) and refused as a value in every project layer, checked file by
file before merging (a table of that name stays a host section);
`ResultsServer` validated once; `compose_layers_with` as the test seam.
`src/bench/remote.rs` is blocking ureq 3 with rustls, one connection per
attempt, no redirects, an attempt timeout of `wire::CONNECTION_DEADLINE`
(moved there from the server's `Limits`) plus 30 s; an answer classified by
its envelope, else by status alone (bare 4xx unapplied and final, 408 and
429 retryable, anything else uncertain), a transport error unapplied only
for a connection that never existed (a refusal included, by the Linux TCP
and ureq argument in the code). `src/bench/publication.rs` is the latch and
retry admission both destinations share: a retry-start window and a cap
(120 s and 8 for a publication, 20 s and 6 for reads), one attempt past
either for a latched publication, a pending stop ending retries after the
first attempt. `brokkr bench` probes the server (`GET /bench`, protocol
checked) before building; `BenchReader` reads either backend, a remote
`run` answer's `children_loaded` checked; `brokkr env` names the server.
Sparred against a `[results]` table (a host section today), an env var,
and stripping the key at storage; the spar added the per-layer refusal
before merging, the retry-start window with the latched exception, the
narrowed never-sent set, the composition test seam and the probe's
honest scope.

What slice 12 left, none planned:

- Runs already in a local `bench.db` stay there once a server is
  configured; pushing them would read whole documents with `Store::load`
  and publish each (small, if the user wants it).
- A listing costs one `run` request per run; a batch read is a new
  protocol version (small, when a remote `results` is too slow).
- A write kind that deletes runs (an `invalidate` for script benches) would
  need a retained request record, since a deleted run cannot answer its
  replay (`reference/bench-store.md`, Publication).

Slice 6, params and `from` resolution (`docs/brokkr.toml.bench.md`, Binding
params and The resolved view; `docs/commands/bench.md`;
`reference/bench-resources.md`, The run's inputs): `src/bench/view.rs`
builds the resolved view from the composed table, hostname and project
root (what the run stores; the view is not stored): host sections dropped
and this host's selected (`datasets` top level, the rest as `host` with
`name` and absolute dirs), file entries absolute (`file` under `datasets`
against the data dir, under `data` against the project root; a plain
dataset's `path`), `snapshot.base`, and every file entry recorded by
segmented path with its pin's raw config path. `src/bench/params.rs`
parses the tokens by hand, binds in dependency order (given, else default,
else unbound and absent), refuses at run time an unknown key with the valid
ones, a default not under its table only when used, a dotted key only where
substituted, a path needing an unbound param, and every `,`, a `from`
param's `all` and a `many` param's `..` (a `many` param binds one key as a
collection of one); bound files are held for reading (`reservations::
hold_reads`, an untied read lease failing closed without a store, held to
the run's end) and then hashed, a pin checked file-only for `file` and
file-or-tree for `path`. `bench(params, keys)` gets each entry with a
bound file's field replaced by its input handle and the chosen keys;
`config()` a fresh copy of the view, files as plain strings. `NAME --help`
lists each `from` param's keys under its parents' given or default keys.
Sparred against a typed view from `DevConfig`, a raw walker without a
view, and every view file entry as a handle; the spar added plain
datasets as inputs, the run-level read hold before hashing, the
substitution-only dotted rule, help only in name position, no invented
`base.seq`, segmented node paths and refusing commas in plain values too.

What slice 6 left:

- The read-only veto against an input is tested at the resource layer
  only: `binary()` has no `writes` list until slice 8, so no script can
  declare an input written yet; slice 8's call options should test it
  through the runner (small).
- A plain dataset's `path` pairs on its digest value alone, so a file
  replaced by a directory with an equal tree digest would pair; the typed
  layer has the same limit (`Dataset::path`). Recording the shape would be
  a new pairing algorithm (small, if ever wanted).
- `config()` hands file entries as strings, so slice 8's `read()`,
  `write()` and `filesize()`, which take handles, reach a config file only
  through a bound param; a script that needs to read an unbound config file
  would want a handle-making builtin, with its own reservation and digest
  story (not planned).

Slice 7, collections and fan-out (`docs/brokkr.toml.bench.md`, Binding
params; `docs/commands/bench.md`, Arms; `reference/bench-resources.md`,
The run's inputs): every param may repeat and every value splits on `,`,
for plain params too (`--jobs 1,2,4` fans out; no value holds a comma, a
comma-bearing table key is reached only through `all`); a `from` param's
`all`, standing alone, is every key in key order (numeric by length then
digits when every key is a canonical decimal, byte order otherwise, the
order every printed key list now uses); `LO..HI` is a range only for a
`from` collection (canonical u64 ends, ascending in place, every key
required, the gaps named from the present keys so a 2^64-wide range costs
nothing), refused for a plain collection, and part of the key for any other
param; duplicates refused after expansion; a default may be a list or
`all`. `params::bind` returns every arm, expanded in `params::order` with
parents outer, a dependent resolved per parent binding; every refusal
across the arms is collected (a refused param is unavailable in its arm, its
dependents not judged there), one returned as itself, several as one
preflight list. A fan-out over no keys under some parent is refused; a
collection's `all` over an empty table binds an empty collection. One read
hold over the union of every arm's inputs, then digests: each file hashed
once, each entry's shape and pin judged on its own. Each arm is a whole
slice-5 run (its own uuid, start, dirty and host reading, worktree, a build
that is a no-op after the first, iterations, publication); a failed arm,
worktree or build included, does not stop the rest, while a stop, a failed
publication and, without `--commit`, HEAD having moved off the invocation's
sha end it, the arms left named as not run and not stored; a stop during
the last arm's publication ends nothing. `--help` lists keys per path the
ancestors lead to. Sparred against one shared worktree and build for all
arms (start times excluding the build, N failed documents per build
failure) and stopping after a build failure (the governing keep-going rule
names no exception, and a build can fail transiently); the spar added the
per-binding pin check under a shared hash, each arm's own ordered inputs
beside the union hold, refusals kept past an independent param's failure,
the stop check between arms, the HEAD check and the cause line per failed
arm.

What slice 7 left, with sizes:

- No test drives `runner::run`, so the arm loop (stop rules, not-run naming,
  the HEAD check, the summary) is exercised only by hand; the binding,
  ordering and digest layers are unit tested. An end-to-end runner test
  needs a fixture project that cargo builds inside the test (medium; slice
  10's first real benches exercise it meanwhile).
- The HEAD check runs between arms only; within one arm, as in slice 5, a
  checkout moving during the build is recorded under the sha resolved
  before it (small: re-resolve after the build and fail the arm).
- A plain collection's `..` stays refused, open for a bench that wants
  integer ranges without a table (small).
- `brokkr results --param` joins a collection's keys with `,`, a lossy
  projection now that `all` can bind a comma-bearing key (documented in
  `docs/commands/results.md`; a structured selector would be new syntax,
  not planned).

8. **Conveniences.** The file and json builtins, call options (`env`,
   `reads`, `writes`; slice 5's `binary()` takes positional words and flag
   tables only, so a path a child sees in another spelling cannot be
   declared yet), feeding one result to another (the spool handle exists,
   `resources::spool_handle`), error line numbers mapped into inline
   scripts (slice 5 names the chunk `[bench.NAME] script`, its lines
   counting from the string's first).
   `ConfigSources::value_origin` names the file that set `script`, but the
   composed table keeps no spans and TOML strings have escapes and line
   continuations, so the mapping re-reads that file with spans
   (`toml_edit`) and decodes the string, after checking the bytes still
   match the script loaded, not offset arithmetic. `read`, `write` and
   `filesize` must act on what a handle names now and stay inside what
   the iteration holds: authorize the current target, as call admission
   does, and open without following a link substituted since (a child can
   turn an output into a link to an input); `write` takes output and
   scratch handles only (`Handle::writable`). Input handles exist since
   slice 6 (bound file params, the iteration's first handles); a `writes`
   entry naming one is refused by `resources::call_access`, and a test
   through the runner should show it.
9. **Evidence views of a script-bench run.** The rest of slice 9: the
   store holds every child's samples, FIFO records, capture loss and clock
   check, and `brokkr results <uuid>` shows only their counts. What
   `brokkr sidecar` shows for a typed run (phases, samples, markers,
   counters, stalls, `--compare`) is wanted per child of a bench run's
   iteration, read through `BenchReader`'s run read (`Store::run(uuid,
   true)` locally, a `run` request when a results server is configured,
   slice 12); `brokkr sidecar`
   resolves through `RunStore`, which does not read the bench store, so
   this is a selection boundary to design, not a flag. One launch,
   smaller than the slices around it; best after slice 10 shows which
   views a real bench needs.
10. **The first real benches.** dellingr's workloads and pbfhogg's
    read-only commands, mostly in their brokkr.toml, shaking out slices 3
    to 8.
13. **The interrupt and the deadline.** Bump dellingr to the release that
    carries the interrupt (`StateBuilder::interrupt` / `State::interrupt`,
    `ErrorKind::Interrupted`, sticky, polled on backward jumps and call
    admission), wire brokkr's Ctrl-C and `brokkr kill` into it so pure Lua
    and top-level code stop at once, map `Interrupted` to an interrupted
    sample in `src/bench/lua.rs`, and define a per-iteration deadline (a
    `--deadline` before NAME or a bench key; none exists in slice 5) that
    covers state setup, top level and `bench()` through the same interrupt
    and kills a running child's group through the supervisor's limit. Test:
    a `while true do end` bench stopped by a stop request and by the
    deadline, stored as interrupted and failed. Small; then
    `docs/commands/bench.md`'s Interrupting a run drops its gap.

Outside brokkr, dellingr's interrupt (dellingr#9) is its own launch in
dellingr, before slice 13: it is in dellingr's tree, not yet released.

## A negative answer that is itself the bug

bz 65 point 2. `check-refs` on an honest dataset should exit 0; a regression
that makes it report false violations exits 1 and is stored as a completed
run with a verdict. The same for a diff of two inputs expected to be
identical. Whether "no" is wrong depends on the input, not the binary, so no
contract mechanism catches it. It needs brokkr to know expected verdicts
(a primary or repack snapshot checks clean, a `degrade --corrupt-*` snapshot
has violations, two different snapshot keys differ), store a run when the
exit matches and fail it when it does not, an exit 0 where violations were
expected included. Some of that is not recorded yet, such as which
`degrade` flags produced a snapshot. Its own ticket.

Landed for bz 71 as declarations, not inference: an input declares its
answer in brokkr.toml and only that answer completes a run (pbfhogg.md,
Declared answers). Three of the proposed defaults did not survive checking:
a primary is not known clean without a key saying so (assuming it would
change what existing configs mean), two snapshot keys do not imply differ
(a repack of `base` is the same), and brokkr's `degrade` has no
`--corrupt-*` flag, while `--unsort` violates ID order only on inputs large
enough. Only `repack --as-snapshot` derives a declaration, `same` against
its source.

## Exit 3

Agreed with the pbfhogg dev on bz 65: exit 3 (the host refused
`--direct-io` or `--io-uring`) stays a failure but gets a named message,
"feature unavailable on this host", so it is not read as a code bug. No row
is stored.

## Where this leaves bz 65 point 1

Commits before the exit split must treat exit 1 as a failure. For the
typed layer the stopgap landed as a gate keyed neither on version nor on
ancestry but on behaviour: `PbfhoggBinary::splits_answer_from_failure`
asks the built executable how it exits on a missing input (1 before the
split, 2 after) and caches the answer under the executable digest. The same
probe-the-binary pattern decides `--overwrite`, `--staging-dir` and
`--tmp-dir`. It cannot help where the flag still parses but a later check
refuses it, which is how bz 73 slipped through. A script bench says it
with `since("854f9465")`.

## Drift in the projects' history

From a read-only survey of every driven project (2026-11-01). Each has
several incompatible shapes in its history, which is what `since()` and
the build's ancestry-keyed overrides have to express.

- **pbfhogg:** the CLI crate split (ed5f0365, 2026-02-25); `bench-*`
  subcommands added (3a2306ee); 22 commands consolidated into 14
  (8f1aad9c, 2026-03-06: `merge` to `apply-changes`, `check-refs` to
  `check --refs`, `merge-pbf` to `cat --dedupe`, and more), which is why a
  bench name must not be the subcommand; `-j` added per command through
  April; `--summary` renamed (b8ac693b); `--index-type dense` removed
  (b70dd8ca); the exit split (854f9465); one flag vocabulary (6e87c0ef);
  OSC diff exit 1 and name-framed OSC output (50a9fc06, 46b48840);
  `--staging-dir` (568b6e3f); usage exit 64 (d2d8836a); `digest --form`
  and its default flip (16154b0d); merge-changes scratch (c97b9985); text
  diff refuses `--tmp-dir` (587bfc12).
- **elivagar:** subcommands added (d3f360c); explicit `--ocean SPEC`
  replacing autodetection and five flags (38250b9, whose message says
  brokkr tilegen fails until it follows); env knobs removed (ab935ff);
  allocator features coming and going; `--dedup-cap` added and
  `--seam-reconcile-layers` removed (0ae55a6, 099611b).
- **nidhogg:** clap replacing hand parsing (9e467ff); the optional disk
  store (fd4b671).
- **dellingr:** the harness took a target name against
  `CARGO_MANIFEST_DIR` before e8799c8 (2026-07-25), so nothing before it
  can be benched; a missing `_bench` exited 0 then and 1 now; FIFO errors
  became fatal.
- **sluggrs:** hotpath features from f619a3e; mandatory `elapsed_ms` from
  29431ea, so every `--bench --commit` before it fails.
- **ratatoskr:** package `app` to `runner` (729eabe4); profile `debug` to
  a custom `harness` profile (5201476f, 2026-10-15).
- **piners:** package `piners-runner` to `piners-harness` (3902a46a1);
  profile `corpus`.
- **mogwai:** `gen --warmup` to `--burn-in` (d3dabc8).

## Bugs found, fixable today

From the same survey, re-verified against the source on 2026-11-04; the
entries since fixed or proved deliberate are gone.

- **`[cargo_env]` commands are unguarded in mogwai and broadarrow:** brokkr
  now has `if_exists`, but `MOGWAI_SOURCE_IDENTITY` and
  `BA_SOURCE_IDENTITY` do not use it, so a `--commit` build older than
  their `scripts/source_identity.py` still fails. The fix is in those two
  configs.
- **ratatoskr stamps markers in UNIX-epoch microseconds**
  (`sidecar_timestamp_us`), while `brokkr man measure` defines the clock
  as starting at process start and marks such runs off-contract. The fix
  is in ratatoskr.
- **mogwai's self-reported `elapsed_ms`** is still emitted and described
  as the benchmark row on mogwai's side (`measure.rs`,
  `reference/performance.md`); brokkr records external wall and drops it.
  mogwai's comments are what is wrong.
- **Stale sluggrs example comments** name `brokkr sluggrs hotpath` and
  `--example email-bench` / `email2-bench` where the examples are
  `email_bench` and `email2_bench`.
- **`--seam-reconcile-layers`** is still emitted when configured, retained
  on purpose for config compatibility and documented as refused by
  elivagar after 099611b; it goes only if the key is retired.
