# pbfhogg project notes

`project = "pbfhogg"` in `brokkr.toml`.

## Module layout

- `src/pbfhogg/commands.rs` - `PbfhoggCommand` enum with typed command
  descriptors, `build_args()`, result identity and observations. Bench and
  hotpath runs share the same argv; mode selects a scratch filename namespace.
- `src/pbfhogg/plan.rs` - resolved command plans for the shared
  `src/measure/runner.rs`; execution uses one `MeasurementSession`.
- `src/pbfhogg/binary.rs` - `PbfhoggBinary`, through which every pbfhogg
  spawn is made; see [Output overwrite](#output-overwrite) and
  [Launch reservations](#launch-reservations-and-the-private-directory).
- `src/pbfhogg/argv.rs` - `PbfArgv`, an argv with what it reads, writes and
  scratches.
- `src/pbfhogg/...` - benchmarks (read, write, merge, commands, extract,
  blob-filter, planetiler, all), verify (one check per
  cross-validated command, plus `all`),
  download (Geofabrik or Planet PBF/OSC fetcher with auto-registration in
  `brokkr.toml`).

## Output overwrite

pbfhogg refuses to write an output path that already exists unless it is
given `--overwrite`, and brokkr reuses its outputs: every `--bench` iteration
writes the same scratch file, and a killed staged write (the merged diff
cache, the indexed PBF a download generates) leaves its partial file for the
retry. So brokkr passes `--overwrite` on every pbfhogg invocation that writes
an output brokkr owns - measured commands, the suite, the benches and every
verify check - and on nothing that only reads.

Old pbfhogg commits, which `--commit` baselines build, predate the flag and
reject it as an unknown argument. The flag is therefore passed only when the
executable about to run accepts it, asked per subcommand of that executable:

- `SUB --help` must exit 0 (the subcommand exists),
- `SUB --hel --help` must exit 2 (an unknown flag there is rejected, so long
  option inference is off and no positional swallows it),
- then `SUB --overwrite --help` exiting 0 means supported, 2 means not.

Anything else is an error naming the probe, never a silent "unsupported".
Answers are kept in `.brokkr/pbfhogg-capabilities`, keyed by probe protocol,
the executable's XXH128 and the subcommand, so a build is probed once and
different bytes never reuse an answer. `build-geocode-index` keeps its own
`--force`, which pbfhogg defines as overwrite plus allow-unindexed.

Every pbfhogg spawn is an invocation produced by a `PbfhoggBinary` for one
build from a `PbfArgv` (`src/pbfhogg/argv.rs`), the argv and what it reads,
writes and scratches built by the same calls: a path goes in as an input, an
output, a directory output or an extract config with the outputs it names, and
only arguments naming no path go in bare. The overwrite decision follows from
that (an argv with outputs negotiates `--overwrite` unless it forces its own),
so the typed commands' arms and the hand-built verify and bench argvs decide it
the same way. A measured plan carries that one argv value: dry run prints it,
and the runner resolves the same value through the handle.

A handle runs the bytes it probed for as long as it exists. It holds the
brokkr lock, so no other cooperating brokkr command can build, and while any
handle is live in the process every cargo spawn through the build seam is
refused before it starts (a handle cannot be made while a build runs, and
builds are exclusive). A flow that builds again ends its handle first, as the
suite does before building the osmpbf baseline. Cargo started any other way
is outside that protocol.

Code under `src/pbfhogg` holds a pbfhogg executable brokkr built only as a
`PbfhoggBinary`. The executable the runner hands a plan is guarded by types:
its path needs a key only the handle can make, and pairing it with argv
directly is refused for a pbfhogg build. The handle and its invocations
expose no path, and reference tools run by a closed name set or as
executables a named resolver produced. The remaining seams that hand out a
built path (the build, harness-build and check modules, cargo spawned
directly, a session's binary) are kept out by two rules in brokkr's own
`brokkr.toml`, `pbfhogg-executables-through-the-handle` for code and
`pbfhogg-executable-seams-not-imported` for imports; being line patterns, they
assume rustfmt layout, do not see an identifier a macro assembles from
separate tokens, and allow no exceptions. The osmpbf baseline builds
through a resolver in `src/tools.rs` whose manifest is fixed. Starting pbfhogg by
bare name from `PATH`, deriving a path from data, or calling another command
whole is outside that claim.

`--dry-run` builds nothing, so it cannot probe: it prints the argv without the
flag and a line saying it is added at run time. Recorded `cli_args` include
`--overwrite` whenever it was passed.

## Launch reservations and the private directory

A pbfhogg child killed mid-write (`brokkr kill`, a timeout, SIGKILL) leaves its
staging file and run scratch behind; pbfhogg reports such leftovers on later
runs and never deletes them. Every pbfhogg launch is therefore reserved
(`src/reservations/`), and one that writes an output or run scratch gets a
private directory of its own.

**The private directory.** Each launch - each child, so every `--bench`
iteration separately - gets a fresh `.brokkr-pbfhogg-<128-bit token>`
directory, passed as `--staging-dir` when the argv writes an output and as
`--tmp-dir` when it writes scratch, so everything disposable the child writes
is in there. It goes in the directory holding the first output (for a
`build-geocode-index --output-dir`, the index directory's parent, never inside
the directory publication replaces), or for scratch with no output (a parallel
text `diff` to stdout) in the root the argv names: the scratch dir for
measured commands, the verify directory for checks. Before anything is
created, every output's directory must be on the same mount as that location,
or the launch is refused, since pbfhogg publishes by rename. Scratch is
declared by what pbfhogg's implementation actually does:
`add-locations-to-ways`, `build-geocode-index`, `diff --format osc` on every
path, and every text `diff` except a quiet one and one with an explicit
`-j 1`, which make no run directory and refuse `--tmp-dir`. A verbose text diff
and one with automatic jobs make one at startup whether or not they write a
shard, which pbfhogg's prose reference does not say. Such a text `diff` to
stdout used to put that run directory in the project root, its working
directory; it now goes to the scratch dir, which can shift measured `diff`
timings when the two are on different filesystems. The argv carries
`@brokkr-private-dir@` until the launch, and the recorded `cli_args` keep that
placeholder (no shell metacharacter, so a pasted line runs), so a row does not
change with the random name; the column differs from rows recorded before
brokkr passed the flags, which carry neither flag.

Both flags are negotiated like `--overwrite`, probed per subcommand as
`SUB --staging-dir=. --help` and `SUB --tmp-dir=. --help` (one run of the two
controls serves all three flags), and kept in the same store under
`SUB --staging-dir` and `SUB --tmp-dir` keys while `--overwrite` keeps the bare
subcommand key it always had. A build that lacks a needed flag (an old
`--commit` baseline) still runs, reserved, without the redirection: its
leftovers land where pbfhogg puts them by default, and the handle says so once
per subcommand.

**The lease.** Before the spawn, brokkr creates `<id>.lock` in
`~/.brokkr/launch-leases/`, flocks it, and writes a durable record of every
path the launch declared - inputs read, outputs written, the private
directory - all synced before the private directory exists. The child, and
only that child, inherits the locked description (it is made inheritable in
the child between fork and exec; every other descriptor brokkr opens stays
close-on-exec), and gets the parent-death signal and the orphan token test
processes carry. After the child is reaped, brokkr gives up its own hold and
takes the lock exclusively from an independent open: that succeeds only when
no process anywhere holds the description, whatever PID namespace it is in.
Then the private directory goes - the one whose identity matches the record,
or, when a crash came between its creation and the record of its identity, the
one at its 128-bit random name - then the record, then the lock file. If the lock is still held after a two-second
window, some process kept the descriptor: the launch fails, naming the lease,
and the record stays as a live reservation. A brokkr killed under a launch
leaves its lease to the next scan, which reclaims it once its child is gone;
the orphan reap at the next hold stops that child, but only the lease proves
it stopped.

A profiled launch (`--hotpath`, `--alloc`) also reserves its hotpath report,
at a path unique per launch, so a profiled child that outlived its run can
never write the next run's report. The report is recorded as disposable, like
the private directory: it goes with the lease, at the release or, if the child
outlived its run, at a later scan.

**What the reservation protects.** Every brokkr deletion, rename or
replacement that can touch these paths - `brokkr clean`'s removals of
pbfhogg's verify output and scratch, a pbfhogg measured run's scratch dirs,
verify's per-check directories, the plan and bench cleanups, the staged
writes of dataset files and of a child's output, snapshot promotion, a
generated extract config, a `--commit` worktree's removal (its target dir,
checkout and slot, `git worktree remove` included, under one exclusive
section) - goes through one gate, which scans the store (reclaiming free
leases on the way) and refuses when a reservation's path is the target, lies
inside it or encloses it, whatever its access: a live reader's input is as
protected as a writer's output. Paths are compared as the kernel resolves
them, every symlink expanded where it is met, and every symlink a reserved
path's resolution goes through counts as part of it, so removing a directory
that holds an intermediate link is refused too; a path that cannot be resolved
is refused. A recursive removal also refuses a tree holding a
`.pbfhogg-backup-*` (matched on the name's bytes), which can be the only copy
of a replaced output; brokkr never deletes one. For a worktree the tree
searched is its target dir's `verify`, where `verify --commit` writes pbfhogg
outputs, not the whole checkout and cargo target dir. `brokkr clean` names a
refused target or worktree and goes on with the rest.

The layer is scoped to pbfhogg. brokkr's own state and config files
(brokkr.toml, journals, caches) are staged plainly, and other projects'
scratch and `brokkr clean` targets are removed plainly, so a lease store
problem never stops them.

A staged write of a file a launch can use is a reservation of its own
(`Staged::stable_reserved`, for dataset downloads and the data dir's
cross-filesystem copy): from before its first byte until its commit or drop it
holds a write reservation on its temp file and its target, so no launch
reading or writing either is admitted meanwhile, and its rename or removal
runs as that reservation is released. `curl`, the download's writer, inherits
the reservation as a pbfhogg child inherits its lease, so a `curl` that
outlives a killed brokkr keeps it held. A temp file a pbfhogg launch writes
itself (the merged-PBF cache, a download's indexed PBF) is staged with
`stable_keeping_for_child`, which holds nothing: the launch reserves it.

Each scan and the admission, release or mutation it decides run in one
critical section: a process-wide mutex and an exclusive flock on the store's
`.serial` lock, so they are atomic across brokkr processes too. A section that
waits two minutes for another process's fails rather than hang outside every
time ceiling. The store directory and every directory newly created above it
are synced before a lease is published. A launch is refused when one of its
paths overlaps a reservation's and either side writes it. A lease store that
cannot be read, or a lease whose record cannot be read, blocks every pbfhogg
launch and gated mutation until a human resolves it; a lease kept after a
failed cleanup blocks what overlaps it. Neither is dropped automatically:
`brokkr clean` names each one with the two files to remove by hand once
nothing uses its paths. The rule `pbfhogg-mutations-through-the-gate` in
brokkr's own `brokkr.toml` keeps raw deletions out of `src/pbfhogg`,
`src/measure` and the staged writer; a raw call there carries a `// gate:`
comment saying why it may.

The guarantee covers cooperating brokkr processes of one user on one host
sharing one `HOME` on a local filesystem with kernel `flock` semantics, the
scope of the global lock and the compile lease, and rests on pbfhogg starting
no helper process that outlives it. Other users, other hosts, bind-mount
aliases of one directory and an NFS `HOME` are outside it.

## Answer exit codes

pbfhogg exits 0 or 1 for a completed command's answer and 2 or above for a
failure: 1 is `diff` finding differences, `check` finding violations,
`inspect --indexed` finding no index, `inspect --show` finding no element.
The measured commands that give such an answer - `diff`, `diff-snapshots`,
`check-refs`, `check-ids` - accept exit 1 as a completed run, so a
`--bench` cell on a file that rightly fails its check (a ways-only snapshot
under `check-refs`) reaches results.db, with `meta.exit_code` recording which
answer was measured (see `docs/commands/results.md`). Exit 2 and signals stay
failures. brokkr's `inspect` never passes `--indexed` or `--show`, so it
accepts nothing but 0.

The set is per command, not "1 is always an answer": pbfhogg commits from
before this contract exited 1 on every failure, and a `--commit` baseline of
`sort` or `cat` must not store a crash as a run. A `--commit` baseline of one
of the four answer commands from that era cannot tell a failure from a
negative answer either way; its `meta.exit_code` of 1 is the only hint.

## Verify subcommands

`brokkr verify` holds the brokkr command lock for the whole invocation,
including `verify all` and its per-check runs, and a `SigtermGuard` alongside
it, so `brokkr kill` or Ctrl-C ends the running pbfhogg/osmium child, prints
`<name>: INTERRUPTED`, stops `verify all` at that check (no further checks, no
tally banner) and exits 130 through the interrupt cleanup.

**Output model.** Every verify check runs "quiet on pass, loud on fail" via
`verify::run_check`: by default a check's detail (`verify_msg` output - section
headers, inspect dumps, element diffs) is captured into a buffer and only
replayed if the check fails; a passing check prints just a one-line
`<name>: PASS (<ms>ms; <steps>)` summary. `<steps>` is the check's wall time
per tool run label, slowest first, with a run count when a label ran more
than once (`osmium sort 2m0s, pbfhogg sort 41.2s, pbfhogg diff 15.1s (2
runs)`), so the summary says how a check's time splits between the pbfhogg
command under test, the reference tool and the comparisons. INTERRUPTED and
FAIL lines carry the same breakdown. While a check runs on a terminal, a live
status line names the step in flight (the same activity `brokkr lock` shows),
so a long osmium step is not silent. `-v`/`--verbose` skips the buffer so
detail streams live even on pass. `verify all` applies this per check, so its default
output is one line per check plus a final tally, and only failing checks spew.
The buffer lives in `output.rs` (`verify_buffer_begin/flush/discard`, fed by
`verify_msg`); one-line results use `verify_summary`, which bypasses it. On
failure `run_check` returns `DevError::ExitCode(1)` so `main` exits non-zero
without re-printing an error it already reported.

**Environment outcomes.** A reference tool (osmium, osmconvert) killed by
`SIGKILL` is not a pbfhogg failure: brokkr never sends a reference tool
SIGKILL (its own stop is a SIGTERM that ends as INTERRUPTED), so the kill
came from outside, commonly the kernel OOM killer on a host too small for
the reference tool at that scale. `run_tool` raises it as
`DevError::VerifyEnvironment`, classified from the child's ending before any
capture failure, and the check prints `<name>: ENVIRONMENT (<ms>; <steps>):
<reason>` with its detail replayed. A comparison failure recorded earlier in
the same check still wins: `Findings::fail` also writes a check-scoped
ledger that an early `?` cannot drop, so the check is a FAIL naming both.
Only `SIGKILL` of a reference tool is environmental; another signal, a
non-zero exit, or pbfhogg itself being killed (its memory at scale is under
test) stays a FAIL. A single check exits 75 (`EX_TEMPFAIL`) on ENVIRONMENT;
`verify all` tallies `environment` beside passed, failed and skipped and
exits 1 if any check failed, else 75 if any was environmental. An
environment kill of osmconvert, which `verify merge` runs for context only,
is reported on the summary channel and changes no verdict.

**Output that scales with the data.** No verify step holds a listing in
memory. Every reference-tool run, and every pbfhogg step whose stdout is a
listing (`diff_pbfs`, renumber's `diff -s -c -v`, `verify diff`'s text
diffs), writes stdout to a file in the check's directory
(`Invocation::run_to_file`, `output::run_stdout_to_file`); stderr stays a
bounded capture. Such a child dies with brokkr (`PR_SET_PDEATHSIG`, refused at
spawn if it cannot be armed), since a file, unlike a pipe, gives an orphaned
writer no reason to stop. Consumers read the files through fixed buffers
(`verify_stream`): per-line heads for renumber's block count, byte-wise
comparison and line counts for `verify diff`, emptiness for `diff_pbfs`'s
verdict. A FAIL replays a bounded head of a listing (lines and bytes per
line) and always names the file holding all of it. Element counts of a
check's outputs come from `inspect --json` written to a file and read by a
streaming parser that keeps only the counts (an inverted getid output keeps
its input's order, so its report is not bounded by being sorted). The human
`inspect` reports that `print_inspect` and `check_sorted` read stay captured,
as do `check --refs` counts; every report consumer still requires its
documented exit codes before trusting what it parsed.

**Verdicts.** A check fails by returning `Err` (a tool crashed, output could
not be parsed) or by returning `Findings` that carry a failed comparison;
`run_check` folds the latter into a FAIL exactly like the former, detail
replay included. The comparison helpers on `VerifyHarness` (`diff_pbfs`,
`check_sorted`, `compare_sort_feature`) return a `#[must_use]` `Verdict`
rather than a `bool`, so a comparison whose result is dropped is a compile
warning instead of a silent PASS - the only way to consume one is
`findings.record(...)`. `diff_pbfs` writes its listing to a file the caller
names in the check's directory and reads `pbfhogg diff`'s exit status: 0 with
no listing is identical, 1 is different, anything else (a signal, another
code) is an error - never "identical". Two comparisons are deliberately not
verdicts: `extract`'s element diff against osmium (known to differ; reported
on the summary channel as informational), and the `dense` index variant of
`add-locations-to-ways`, whose non-zero exit is taken as an allocation
failure on hosts without `vm.overcommit_memory=1` and skipped.

**Fresh outputs.** `VerifyHarness::subdir` empties `target/verify/<check>/`
before handing it back, so every file a check finds there was written by this
run. Checks rely on that: `verify_merge` treats the diff OSC's existence as
proof the diff ran. The optional osmosis/osmconvert outputs are printed only
for a tool whose run succeeded (tracked from the run, not from the file
existing), and a failed optional tool's partial output is removed.

**`verify all` inputs.** The OSC- and bbox-consuming checks skip only when
the input is not configured at all (no `--osc-seq` and no OSC registered for
the dataset or named snapshot; no `--bbox` and no dataset `bbox`). A
configured input that fails to resolve - hash mismatch, missing file, several
OSCs and no `--osc-seq`, a malformed bbox - fails the command before the suite
runs rather than turning its checks into skips. osmosis setup failure is
non-fatal but narrated.

Every verify subcommand that takes `--dataset` also accepts `--input <PATH>`
to skip dataset resolution and use a handcrafted fixture, and `--snapshot
<key>` to cross-validate a registered snapshot (e.g. an adversarial encoding
from `degrade`/`repack --as-snapshot`) instead of the primary data. `--input`
and `--snapshot` are mutually exclusive. PBF resolution is centralised in
`resolve_verify_input` (`src/pbfhogg/cmd.rs`), which routes through
`resolve_snapshot_pbf_path` for both the base and named cases.

OSC resolution for the change-consuming verifies (`merge` / `derive-changes`
/ `diff`) is snapshot-aware via `resolve_verify_osc`: a named snapshot that
carries its own `osc` table (a point-in-time snapshot) is diffed against
*that* chain; an encoding-only snapshot (degrade/repack has no OSC table)
falls back to the dataset's primary chain - the logically-correct diff stream,
since such snapshots are same-sequence re-encodings of the base PBF. Which
chain resolved is narrated to stderr (`[verify] osc: snapshot-scoped (<key>)`
vs `[verify] osc: base fallback (<key> has no osc table)`), but only when
`--snapshot` was actually passed. Even a deliberately misaligned OSC is still
a valid tool-vs-tool check - both pbfhogg and osmium apply the same changes to
the same input - so the fallback never invalidates the cross-check.

`verify_merge` parses the input OSC's delete set via `osc::parse_osc_file` and
runs a strict `pbfhogg diff --format osc` between pbfhogg's and osmium's
outputs - osmium-only IDs that appear in the input OSC's delete set are
exempt (osmium does version-based deletes; pbfhogg/osmosis/osmconvert delete
unconditionally), everything else fails.

`verify merge --osc <PATH>` takes a handcrafted change file instead of
resolving one (it conflicts with `--osc-seq`), the partner of `--input` for a
fixture whose base and diff are built together.

`verify merge --locations-on-ways` runs both tools with `-l`. `pbfhogg diff`
treats inline way coordinates as derived data, not content, so the element
diff cannot see a stale coordinate; instead pbfhogg's output must pass
`pbfhogg check --locations` (each way's inline coordinates equal its nodes'
records in the same file; exit 1 is a FAIL, any other non-zero exit an
error). osmium's output gets the same check, reported on the summary channel
only - a STALE result with the head of the check's report, so a change in
what osmium gets wrong shows from run to run (`-v` streams the whole
report): its location index is searched before it is sorted and resolves
duplicate keys to the smallest location, so multi-version diffs can make
osmium itself write stale coordinates. osmosis and osmconvert, which write no
inline locations, are not run. The base must be LocationsOnWays and keep its
untagged nodes (osmium's `--keep-untagged-nodes`): pbfhogg refuses `-l` with
unresolved refs, and `check --locations` can only judge a ref whose node is
in the file.

`verify all` runs this mode as its own check, `apply-changes -l`, beside the
plain merge: same OSC, but on the dataset's (or the `--snapshot`'s)
`altw-keep-untagged` PBF variant, whatever `--variant` says, since a plain
`altw` variant drops untagged nodes. It skips when no OSC is configured, when
no `altw-keep-untagged` variant is registered, or under `--input` (a fixture
has no keep-untagged partner; run `verify merge --input --osc
--locations-on-ways` instead). A registered variant that fails to resolve
fails the command, as a configured OSC does.

`pbfhogg diff` is a merge-join and refuses an input that does not declare
`Sort.Type_then_ID`. osmium's `apply-changes` output is content-sorted but does
not stamp the feature, so `verify_merge` and `verify_derive_changes` pass it
through `pbfhogg sort --force` (content unchanged) before diffing.

`pbfhogg diff` has a separate code path for inputs without indexdata, meant
to answer exactly as the indexed one does. `apply-changes` refuses an
unindexed base, so `--variant raw` cannot reach it; instead `verify_diff` and
`verify_derive_changes` strip both sides of their verified pair with `pbfhogg
degrade --strip-indexdata` and rerun the diff. `verify_derive_changes`
requires the indexed OSC's decompressed content (gzip framing is not
compared), and `verify_diff` requires the indexed listing byte for byte and
the same summary classes. The indexed answer is the one held to osmium, so
the match holds the unindexed path to osmium too. Both treat exit 1 from
`pbfhogg diff`, text or OSC, as "the inputs differ", and 2 and above as a
failed run.

`verify renumber` requires the comparison diff to exit 0 or 1 and its `-s`
summary (on stderr) to carry all four of `left`, `right`, `same` and
`different`; `same + different` must equal both totals, so equal totals
cannot hide one-sided elements, and the block headers classified by kind in
the listing must number exactly `different` (pbfhogg writes one per modified
element), so a listing the classifier does not fully read fails rather than
passing node or way differences as tolerated relation ones.

`verify getid-removeid` chooses its IDs from the input, not from a fixed
list: a header-only walk of the PBF's blob framing (`pbf_frames`) reads each
`BlobHeader` and its indexdata and seeks past every data payload, so the
choice costs the blob count, never the data, at any scale. For each kind
present it takes the lowest and highest ID the indexdata claims, which exist
in any input whose claims are true, drops either if negative (pbfhogg getid
does not accept negative IDs), and asserts per kind that getid selected
exactly as many as were requested. The walk decodes indexdata in
pbfhogg's wire format (versions 1 to 3) and refuses whatever pbfhogg could not
read through it - a data blob without indexdata, two non-empty indexdata
fields, an unknown version, a length the version does not declare, an
unknown kind, an inverted range - as well as malformed framing; an unknown
future version is refused by name. The one payload it reads is the
`OSMHeader`'s, and a header declaring `HistoricalInformation` is refused,
since the cardinality assertion needs one object per kind and ID. The
original's counts for the complement test are the indexdata claims from the
same walk. The input must not change on disk while the check runs.

`verify add-locations-to-ways` runs osmium with `--keep-member-nodes`: pbfhogg
keeps untagged relation-member nodes unconditionally (a documented pbfhogg
deviation), osmium only with that flag.

## OSC parser (`src/osc.rs`)

Minimal `.osc` / `.osc.gz` reader. Returns `OscDiff` with sorted ID sets per
(`<create>` / `<modify>` / `<delete>`) section per element kind. Used by
`verify_merge` for the delete-set carve-out.

Streaming hand-rolled tag scanner; gzip and plain XML are consumed in bounded
chunks, and the scanner retains only its current tag rather than the entire
decompressed file. It is tolerant of XML comments, processing
instructions, self-closing elements, and single-quoted attributes. Element
bodies (tags / refs / members / coords / metadata) are deliberately skipped -
only IDs are needed. The document itself is checked: the first element must
be the `<osmChange>` root and the root must be closed, with nothing after it.
An empty file, a non-OSC document or a truncated diff is an error, not an
empty `OscDiff` - `verify_merge` would otherwise report it as
"element-identical PASS".

## Snapshots and variant selection

pbfhogg accepts these flags on every measurable command. For the
schema-universal flags (`--variant`, `--osc-seq`, `--tiles`) see
`docs/brokkr.toml.md`. The pbfhogg-only flags are:

- `--snapshot <key>` - selects which snapshot's `pbf`/`osc` tables the
  resolver reads from. `base` (or omitting the flag) reads from the legacy
  top-level data. Any other key reads from
  `[..datasets.<dataset>.snapshot.<key>]`. Accepted by every measurable
  pbfhogg command - the resolver in `build_pbfhogg_context`
  (`src/pbfhogg/plan.rs`) calls `resolve_snapshot_pbf_path` regardless of
  which command is dispatching, so wiring a new pbfhogg command into the
  snapshot graph is just a matter of adding the field to its CLI variant in
  `src/cli/schema.rs` and propagating it to `CommandParams.snapshot` in
  `Command::into_route` (`src/command_input.rs`). The `read` throughput benchmark and the
  `verify` cross-validation suite accept `--snapshot` too (they resolve
  outside `build_pbfhogg_context` but share `resolve_snapshot_pbf_path` via
  `SnapshotRef::from_opt`); the synthetic `write`/`merge-bench`/etc. benches
  do not.
- `--as-snapshot <key>` / `--replace-snapshot` - (`repack`, `degrade` and
  `cat` only) promote the final iteration's scratch artifact into the dataset
  graph as a new snapshot. `cat --type way --as-snapshot ways-only` is how a
  single-kind input (every way ref dangling) is made addressable as
  `--snapshot ways-only`. `--replace-snapshot` allows overwriting an existing key;
  without it, an existing key errors out. Both flags are validated up-front
  via `download::preflight_snapshot_collision` (called from the top of
  `run_command_with_params`), so a forgotten `--replace-snapshot` errors
  before the cargo build kicks off, not after the run.

## I/O and compression flags

pbfhogg-specific flags that adjust cargo features and binary args. Note on
result rows: post-v13 the `mode` column (formerly `variant`) carries only the
measurement mode (`bench`/`hotpath`/`alloc`). These axis flags are **not**
folded into a variant suffix anymore - they live in the `cli_args` /
`brokkr_args` columns and are what the `brokkr results` table's `args` column
renders and what `brokkr results --compare` keys its pairs on. So a flag-on run
and its flag-off baseline at the same commit are distinct rows / distinct
compare pairs automatically, no suffix required.

- `--direct-io` - enable O_DIRECT I/O. Adds `linux-direct-io` cargo feature,
  `--direct-io` binary flag.
- `--io-uring` - enable io_uring I/O. Adds `linux-io-uring` cargo feature,
  `--io-uring` binary flag. Runs io_uring preflight checks before building.
  Only supported by `apply-changes`, `sort`, `cat-dedupe`, `diff-osc`,
  `repack`, and `degrade`; brokkr rejects it for other commands before
  building.
- `--compression <spec>` - output compression passed through to the binary.
  Values: `zlib` or `zlib:N` (1-9), `zstd` or `zstd:N`, `none`. Bare
  `zlib` and `zstd` use the binary's default levels. No cargo features
  required.
- The synthetic `write` and `merge` benches run their own codec matrix and
  refuse all three flags rather than ignore them: pick codecs with
  `--compressions`, and io_uring on `merge` with `--uring`.
- `--inject-prepass` - (`add-locations-to-ways` only) emit the injected-prepass
  wire extensions (BlobHeader field 5 way-member bitmaps, Way field 20
  shared-node pins; declared via the `pbfhogg.WayMembers-v1` /
  `pbfhogg.SharedNodePins-v1` header feature strings). Forwarded verbatim to
  the pbfhogg child (`src/pbfhogg/commands.rs`, `AddLocationsToWays` arm), no
  cargo features. Composes with `--index-type sparse|external|auto`,
  `--bench`/`--hotpath`/`--alloc`, `--commit`, `--compression`, `--snapshot`,
  and the I/O flags. pbfhogg hard-errors on invalid combinations (e.g. sparse
  without indexdata), so brokkr does no validation of its own beyond
  forwarding. The producer's four counters (`altw_member_ways`,
  `altw_pinned_refs`, `altw_field5_bytes`, `altw_field20_ways_emitted`) ride
  the existing sidecar FIFO counter channel and show up in
  `brokkr sidecar --counters` unchanged. **`brokkr verify
  add-locations-to-ways --inject-prepass` is refused** (nonzero exit, no diff
  run): enriched output is osmium-incompatible by design (field-5 headers run
  ~1-8 KB; libosmium 2.23 rejects any BlobHeader over 127 bytes, their issue
  405), so there is no reference tool to cross-validate against. Run flag-off
  verify for the element semantics; enriched correctness is covered by
  pbfhogg's own oracle-roundtrip + backend-parity suite.
- `--force-altw` - (`add-locations-to-ways` only) appends `--force` to the
  pbfhogg child (`src/pbfhogg/commands.rs`, `AddLocationsToWays` arm), skipping
  its indexdata requirement so raw / non-indexed input reaches the decode-all
  fallback path. Named to disambiguate from brokkr's own per-subcommand
  `--force` dirty-tree override, mirroring `--force-repack`. Composes with
  `--inject-prepass`, `--index-type`, `--compression`, and the measurement
  modes. brokkr does no validation of its own - pbfhogg owns the semantics. The
  intended cell: `brokkr add-locations-to-ways --dataset europe --variant raw
  --index-type sparse --compression zstd:1 --force-altw --bench 3`.
- `--locations-on-ways` - (`apply-changes` only) passes through to the child
  pbfhogg invocation. Like `verify merge --locations-on-ways`, it needs a
  LocationsOnWays base that keeps its untagged nodes; a default altw variant
  drops them and pbfhogg refuses the run.

### Durable enriched output (re-enrichment workflow)

`add-locations-to-ways` writes to scratch and cleans up after every
run/bench iteration - the output does not survive by design. To produce an
enriched file that survives (e.g. to register as a dataset's `locations`
variant for elivagar's `tilegen --variant locations`), run the producer
through the raw passthrough, which does no cleanup:

```
brokkr passthrough add-locations-to-ways <input.osm.pbf> \
  -o <durable/out.osm.pbf> --index-type external --inject-prepass \
  --compression zlib:6
```

then register the file manually in `brokkr.toml` under
`pbf.locations` with a `brokkr env` xxhash. (There is no `--as-snapshot`
promotion for `add-locations-to-ways`: that machinery routes into the
`snapshot` graph under `pbf.indexed`, which is the wrong target for a
top-level `locations` variant.)

## download command

`download <region> [--osc-seq N]` - download PBF + OSC from Geofabrik or the
Planet replication service. Geofabrik accepts short aliases (`denmark`,
`europe`) or full paths (`europe/france`, `asia/japan/kanto`). Dataset key is
the last path component.
Checks configured filenames in `brokkr.toml` before downloading. `--osc-seq N`
downloads all missing diffs from `last_configured_seq + 1` through N. After
downloading, computes xxh128 hashes and registers the new entries in
`brokkr.toml`. PBFs are named `{key}-{YYYYMMDD}.osm.pbf` (the download day)
or `{key}-{snapshot}.osm.pbf`. OSCs are named `{key}-seq{N}.osc.gz` in every
flow: the only date at hand is the download day or the snapshot key, neither
says what edits a diff holds, and a dated name invited applying a diff to the
base of that date. Files registered under the older `{key}-{date}-seq{N}`
names keep working - resolution reads the registered filename. The indexed variant is generated with plain
`pbfhogg cat <raw> -o <indexed>` (`generate_indexed_pbf`, shared by the
primary, `--as-snapshot` and `--refresh` flows).

Every `brokkr.toml` change a download flow (or `--as-snapshot` promotion)
makes goes through one `toml_edit` transaction (`DatasetToml` in
`src/pbfhogg/download/registry.rs`): keys and values are escaped by the
writer, comments and unrelated tables survive untouched, and the batch is
committed once with an atomic replace, only after every file it names exists
and is hashed. A failure before the commit leaves `brokkr.toml` unchanged -
in particular `--refresh` never leaves a dataset with its primary rotated out
and no replacement registered. `--replace-snapshot` unlinks the displaced
snapshot's files only after the new registration is committed, and keeps any
file the primary or another snapshot still names. `--refresh` refuses when
today's dated filenames are already registered (a same-day refresh would
overwrite the primary it is archiving).
