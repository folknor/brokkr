# pbfhogg project notes

`project = "pbfhogg"` in `brokkr.toml`.

## Module layout

- `src/pbfhogg/commands.rs` - `PbfhoggCommand` enum with typed command
  descriptors, `build_args()`, result identity and observations. Bench and
  hotpath runs share the same argv; mode selects a scratch filename namespace.
- `src/pbfhogg/plan.rs` - resolved command plans for the shared
  `src/measure/runner.rs`; execution uses one `MeasurementSession`.
- `src/pbfhogg/binary.rs` - `PbfhoggBinary`, through which every pbfhogg
  CLI spawn is made; see [Output overwrite](#output-overwrite) and
  [Launch reservations](#launch-reservations-and-the-private-directory).
- `src/pbfhogg/argv.rs` - `PbfArgv`, an argv with what it reads, writes and
  scratches.
- `src/pbfhogg/...` - benchmarks (read, write, merge, commands, extract,
  blob-filter, planetiler, all).

Library verification, dataset download and snapshot promotion, and the
cross-validation checks are not brokkr commands: they live in pbfhogg's own
binaries (`pbfhogg-library`, `pbfhogg-datasets`, `pbfhogg-verify`), run
through `brokkr run --with`, for example `brokkr run --with pbfhogg
pbfhogg-verify -- CHECK`. Library measurement is the `library-scenario`
script bench.

## Output overwrite

pbfhogg refuses to write an output path that already exists unless it is
given `--overwrite`, and brokkr reuses its outputs: every `--bench` iteration
writes the same scratch file, and a killed staged write (the merged diff
cache) leaves its partial file for the
retry. So brokkr passes `--overwrite` on every pbfhogg invocation that writes
an output brokkr owns - measured commands, the suite and the benches - and on
nothing that only reads.

Old pbfhogg commits, which `--commit` baselines build, predate the flag and
reject it as an unknown argument. The flag is therefore passed only when the
executable about to run accepts it, asked per subcommand of that executable:

- `SUB --help` must exit 0 (the subcommand exists),
- `SUB --hel --help` must exit with a usage error (an unknown flag there is
  rejected, so long option inference is off and no positional swallows it):
  2, clap's own status, or 64 (`EX_USAGE`) from a pbfhogg build that splits
  usage errors from runtime failures, which keep 2; the status it shows is
  that build's usage error,
- then `SUB --overwrite --help` exiting 0 means supported, and exiting with
  that same usage error means not.

Anything else is an error naming the probe, never a silent "unsupported".
Each probe runs in its own process group with a 60-second wall and without
`BROKKR_MARKER_FIFO` or `BROKKR_HARNESS_ARTEFACT_DIR`, so a probe never
opens a marker FIFO brokkr inherited; a probe killed at its wall, or whose
output stays open after it exited, is such an error too.
Answers are kept in `.brokkr/pbfhogg-capabilities`, keyed by probe protocol,
the executable's XXH128 and the subcommand, so a build is probed once and
different bytes never reuse an answer. The same store and bounds serve one
more question, whether the build's exit 1 is only an answer (Answer exit
codes, below). `build-geocode-index` keeps its own
`--force`, which pbfhogg defines as overwrite plus allow-unindexed.

Every pbfhogg CLI spawn is an invocation produced by a `PbfhoggBinary` for one
build from a `PbfArgv` (`src/pbfhogg/argv.rs`), the argv and what it reads,
writes and scratches built by the same calls: a path goes in as an input, an
output, a directory output or an extract config with the outputs it names and
the polygon files it reads, and
only arguments naming no path go in bare. The overwrite decision follows from
that (an argv with outputs negotiates `--overwrite` unless it forces its own),
so the typed commands' arms and the hand-built bench argvs decide it
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
`PbfhoggBinary`, which holds the lock and the build registration, re-stats
its executable before each spawn and exposes no path. The executable the runner hands a plan is guarded by types:
its path needs a key only the handle can make, and pairing it with argv
directly is refused for a pbfhogg build. The handle and its invocations
expose no path. The remaining seams that hand out a
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
text `diff` to stdout) in the root the argv names. Before anything is
created, every output's directory must be on the same mount as that location,
or the launch is refused, since pbfhogg publishes by rename. Scratch is
declared by what pbfhogg's implementation actually does:
`add-locations-to-ways`, `build-geocode-index` and `diff --format osc` on
every path, and `merge-changes` only where it can spill: several inputs, no
`--simplify`, and a `--jobs` other than 1 (an omitted or 0 `--jobs` is auto
and counts, whatever it resolves to on the host; `commands::merge_changes_spills`
is the one predicate). Since pbfhogg 2b1c6ba1 only
those make a run directory, and every other merge-changes refuses `--tmp-dir`
as a usage error. Builds from c97b9985, which gave merge-changes `--tmp-dir`,
up to 2b1c6ba1 made a run directory on every merge-changes; the flag probe
cannot tell them from later builds, so on the other shapes such a `--commit`
baseline puts its run directory beside the output, removed on completion and
left behind by a killed child. A build before c97b9985 lacks the flag and
makes no run directory. A text `diff` writes no
scratch since pbfhogg 587bfc12 and
refuses `--tmp-dir` as a usage error, so brokkr never declares scratch for
one; a `--commit` baseline from before that commit puts its text-diff scratch
wherever pbfhogg defaults it. brokkr spells every
pbfhogg flag it forwards long (`--output`, `--jobs`, `--invert-match`, ...),
since pbfhogg has reassigned short letters between versions and a `--commit`
baseline accepts the long forms. Rows recorded earlier carry short spellings in `cli_args`. The argv carries
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
leftovers land where pbfhogg puts them by default, and the handle names the
missing flags once per subcommand. It does not claim leftovers exist: a build
older than a command's scratch lacks `--tmp-dir` because it writes none.

**The lease.** Before the spawn, brokkr creates `<id>.lock` in
`~/.brokkr/launch-leases/`, flocks it, and writes a durable record of every
path the launch declared - inputs read, outputs written, any coverage, the
private directory - all synced before the private directory exists.
A launch may also declare coverage, a tree reserved for writing around its own
paths. It is reservation only: the private directory's placement compares
outputs alone, and the release never removes it. The child, and
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
pbfhogg's leftover output and scratch, a pbfhogg measured run's scratch dirs,
the plan and bench cleanups, the staged
writes of a child's output, a generated extract config, a `--commit` worktree's removal (its target dir,
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
searched is its target dir's `verify`, where older runs wrote pbfhogg
outputs, not the whole checkout and cargo target dir. `brokkr clean` names a
refused target or worktree and goes on with the rest.

The layer is scoped to pbfhogg and to script-bench iterations, which share
the store: an iteration's owner lease and its children's borrows are leases
too, held and reclaimed by the same scans, with a held borrow keeping its
free owner (`reference/bench-resources.md`). Every recursive removal of a
configured directory, in any project, goes through the gate without the
backup walk, since a script-bench iteration can sit under one: the measured
runner's scratch trees, elivagar's scratch, `tilegen_tmp` and calibrands,
nidhogg's ingest output. brokkr's own state and config files (brokkr.toml,
journals, caches) are staged plainly, and its fixed-name `brokkr clean`
targets are removed plainly, so a lease store problem never stops them.

A temp file a pbfhogg launch writes itself (the merged-PBF cache) is staged
with `stable_keeping_for_child`, which holds no reservation of its own: the
launch reserves it, and its commit and drop go through the gate.

Each scan and the admission, release or mutation it decides run in one
critical section: a process-wide mutex and an exclusive flock on the store's
`.serial` lock, so they are atomic across brokkr processes too. A section that
waits two minutes for another process's fails rather than hang outside every
time ceiling. Every directory from the home down to the store is synced before
each lease is published, whether this launch created it or an earlier one did.
A launch is refused when one of its
paths overlaps a reservation's and either side writes it. A lease store that
cannot be read, or a lease whose record cannot be read, blocks every pbfhogg
launch and gated mutation until a human resolves it; a lease kept after a
failed cleanup blocks what overlaps it. While a lease cannot be read, no
free script-bench iteration is reclaimed either, since the unreadable one
may be a borrow of it. Neither is dropped automatically:
`brokkr clean` names each one with the two files to remove by hand once
nothing uses its paths. The rule `pbfhogg-mutations-through-the-gate` in
brokkr's own `brokkr.toml` keeps raw deletions out of `src/pbfhogg`,
`src/measure` and the staged writer; a raw call there carries a `// gate:`
comment saying why it may.

The guarantee covers cooperating brokkr processes of one user on one host
sharing one `HOME` on a local filesystem with kernel `flock` semantics, the
scope of the global lock and the compile lease, and rests on pbfhogg starting
no helper process that outlives it (for a script bench, on its children
keeping the borrow they inherited). Other users, other hosts, bind-mount
aliases of one directory and an NFS `HOME` are outside it.

## Answer exit codes

pbfhogg exits 0 or 1 for a completed command's answer and 2 or above for a
failure: 1 is `diff` finding differences, `check` finding violations,
`inspect --indexed` finding no index, `inspect --show` finding no element.
The measured commands that give such an answer - `diff`, `diff-snapshots`,
`check-refs`, `check-ids`, `check-index`, `check-locations` - accept exit 1 as a completed run, so a
`--bench` cell on a file that rightly fails its check (a ways-only snapshot
under `check-refs`) reaches results.db, with `meta.exit_code` recording which
answer was measured (see `docs/commands/results.md`). Exit 2 and signals stay
failures. brokkr's `inspect` never passes `--indexed` or `--show`, so it
accepts nothing but 0.

The set is per command, not "1 is always an answer": pbfhogg commits from
before this contract exited 1 on every failure, and a `--commit` baseline of
`sort` or `cat` must not store a crash as a run.

It is also per build. pbfhogg 854f9465 moved failures from 1 to 2 and left
the answer sites on 1; a build from before it cannot tell "no" from
"crashed" by its exit. So before an answer command runs, brokkr asks the
built executable which side of the split it is on, the way it asks about
`--overwrite` (`src/pbfhogg/binary.rs`): `inspect` on an input that does not
exist fails with 1 before the split and 2 after it, and the answer is cached
under the executable's digest in `.brokkr/pbfhogg-capabilities`. On a build
from before the split exit 1 is not accepted: a run that ends 1 fails, and a
warning says the build predates the split. Its negative answers are lost
with its crashes; no run is stored that might be either. Rows stored before
this gate from such a build keep their `meta.exit_code` of 1, which there
cannot say which it was.

`diff --format osc` exited 0 on differences until pbfhogg 50a9fc06, so a
build between the two never gives it exit 1; it needs no gate of its own.

A row from a build that predates the split records `meta.exit_code` 0, as
any answer command's completed run does; rows stored before this recorded
no key there.

### Declared answers

Accepting both answers is wrong when no is the bug: `check-refs` on an
honest file that starts reporting false violations exits 1 and would be
stored as a completed run. An input whose answer is known can say so in
brokkr.toml (`docs/brokkr.toml.datasets.md`), and then only that answer completes the
run: `clean` or `same` accepts exit 0 alone, `violations` or `differs` exit
1 alone, and the other answer fails the run like a crash. An input that
declares nothing keeps both answers; nothing is inferred, not "the primary
is clean" and not "two snapshot keys differ", which a repack of `base`
disproves.

- `check-refs`, `check-ids` (with or without `--full`) and `check-index`
  read `[..pbf.<variant>.expect]` on the entry the command resolved, primary
  or `--snapshot`, so each variant file declares for itself.
  `check-refs --check-relations` reads `check-refs-relations` there, never
  `check-refs`: clean way refs say nothing about relation members.
  `check-locations` reads `check-locations` and `check-locations --agreement`
  reads `check-locations-agreement`, the same split.
- `diff` reads `diff."<seq>"` on the same table: this file against itself
  with that OSC applied. A declared `diff` rebuilds the merged B side with
  the build under test even under `brokkr run` or `--keep-cache`, since a
  cached merge is keyed by name, not by the OSC's content or the build that
  made it. The suite always rebuilds it.
- `diff-snapshots` reads `[..datasets.<name>.expect.diff-snapshots]`, keyed
  by the registered digests of both files, so a rotation of a
  snapshot file can never point a declaration at another file. A side
  without an `xxhash` cannot be addressed and stays undeclared.

The accepted exits are the answers the build can be trusted to give
intersected with the declared one. A build from before the split, asked to
answer `violations` or `differs`, leaves nothing to accept: the run is
refused before it starts. A declared run says what it expects before it
runs, and a dry run lists it. The suite reads the declarations of the
primary entry it measures.

brokkr records no declaration itself, and no check verdict is ever derived:
a repack can repair the metadata its input lied about. Snapshot promotion is
pbfhogg's own tool now (`brokkr run --with pbfhogg pbfhogg-datasets --
promote ...`), and it is that tool's to say what it records.

## Diff text output

The text arm of `brokkr diff` and `brokkr diff-snapshots` (no `--format osc`)
runs `pbfhogg diff A B --suppress-common --output FILE`, writing the text to
`<scratch>/bench-<id>-output.txt` (`hotpath-` under `--hotpath` and
`--alloc`), removed before and after the run and by `brokkr clean`, like the
OSC arm's `.osc.gz`. The text never passes through stdout, whose capture is
bounded (`brokkr man measure`, trajectory capture limits): a planet diff
between snapshots weeks apart writes far more text than the bound, and was
cut off and stored nothing. Writing the text stays part of the measured cost,
so `-q`, which only counts and skips byte-equal blobs, is never passed.
pbfhogg publishes the file on both answers, exit 0 and exit 1.

Rows recorded before this streamed the text to stdout. Their brokkr argv is
the same, so they pair with newer rows in `brokkr results` comparisons, but
they did not pay for staging, sync and publication of the file; only
`cli_args`, which gains `--output`, tells the two apart. The planet rows of the
stdout era never completed.

`brokkr diff` builds its merged side with `pbfhogg apply-changes`
(`ensure_merged_pbf`, `src/pbfhogg/plan.rs`), which refuses a base without
blob-level indexdata. brokkr reads the base's leading blob header, and when
it carries no indexdata it passes `apply-changes --force`, so `--variant raw`
runs. That merge writes indexdata on every blob, so the pair diffed is raw
vs indexed, a mixed pair pbfhogg sends down its sequential path: the row
records `meta.diff_pair=raw-vs-indexed`, absent on an indexed pair as on
every row stored before the key. `--variant raw` is already in the argv, so
the arm key separates the two without the key. A base whose header the probe
cannot read is merged without `--force`, with a warning. The suite's
`diff` presets share the probe.

## Digest

`brokkr digest` measures `pbfhogg digest` on the dataset PBF like any other
read-side command (`--bench`, `--hotpath`, `--alloc`, `--commit`).

The form is a workload axis brokkr holds constant across revisions, not a
default it leaves to the build. pbfhogg 16154b0d added `--form` and, in the
same commit, moved its default from v1 content to v2 full; every `digest`
before it has one form, v1 content. Leaving the choice to the build would
time a different function on either side of that commit under one identity.
So before the run brokkr asks the built executable whether its `digest`
takes `--form` (`digest --form=full --help` against the usual controls,
cached under the executable's digest like the other capability answers in
`.brokkr/pbfhogg-capabilities`), and then:

- without `--form`, a build that takes it gets `--form full`, spelled so a
  later default change cannot move it, and one that does not gets no flag
  and digests content;
- `--form content` is forwarded where the build takes it and dropped where it
  does not, since content is that build's only form, so a content baseline
  from before 16154b0d pairs with a content run after it;
- `--form full` is always forwarded, and a build without the flag refuses it.

That a build without `--form` digests content is pbfhogg's history, not
something the probe proves. A dry run builds nothing to ask, so it prints the
argv for the form asked, and, without `--form` or with `--form content`, a
line saying what the build decides.

The brokkr argv, `--form` included, is part of the row's identity, so an
explicit `--form` run never pairs with a defaulted one. A defaulted row
carries the form it measured as its qualifier (`form=full` or
`form=content`), and every digest row whose form was resolved records it as
`meta.digest_form`, so `--meta digest_form=content` selects across both
spellings. A defaulted row from before this carries neither and pairs only
with other such rows; that set mixes the two forms and nothing recorded
tells them apart (its `commit` against 16154b0d does).

## Inspect tags

`brokkr inspect --tags` measures `pbfhogg inspect tags PBF [EXPR...]
[--type KIND] --min-count 999999999`. pbfhogg applies the threshold after
the exact counts are known and keeps rows at or above it, which no row of a
real input is meant to reach, so the run pays for decoding and counting and
prints no count rows, only its summary line; a row counted that often would
still print. The threshold never bounds the count table.

What is counted is the build's, not brokkr's, unless an expression says.
pbfhogg 9c4d6c81 made a bare run count keys (osmium tags-count parity); before
it a bare run counted every key=value pair. At planet those are different
workloads, about 108 thousand keys in 0.4 GB against 187 million pairs in
10 GB, under one brokkr argv and so one results identity. A bare
`--compare` across 9c4d6c81 compares the two workloads, not one workload's
speed, and stored rows are not marked: a row's `commit` against 9c4d6c81 is
what tells them apart. The suite's `inspect-tags` and `inspect-tags-way`
presets and the blob-filter bench's `inspect-tags-way` are bare too, and move
with it.

`--expr EXPR` (repeatable, `--tags` only) forwards pbfhogg's positional tag
expressions in order, right after the PBF. It is in the brokkr argv, so each
spelling keys apart. `--expr '*=*'` counts every pair at every revision with
expressions, which is the work bare runs did before 9c4d6c81; no spelling
counts keys before it, where an expression without `=` counted that key's
pairs. To compare the pair workload across the change, measure both commits
with the same `--expr '*=*'`: an older bare row never pairs with an explicit
one. An expression starting with `-` is refused, since brokkr appends its
flags after the expressions and pbfhogg would read it as an option; the rest
of the grammar is the build's to accept. Quote expressions holding `*`.

The suite's `inspect-tags-pairs` preset runs `--expr '*=*'`, so the suite
keeps covering the pair table. Suite presets that share a command id carry
their preset name in the row (`brokkr man results`, `--compare A B`).

## Getid add-referenced

`brokkr getid --add-referenced` measures `pbfhogg getid --add-referenced`
on the dataset's `getid_ids`, else the built-in list, which holds three
relations. What it follows is the build's: from pbfhogg d4c97d42 it follows
relation members recursively as osmium `getid -r` does, before it only a
requested way's nodes. The argv is the same on both sides, so a `--compare`
across d4c97d42 compares two workloads, and stored rows are not marked: a
row's `commit` against d4c97d42 tells them apart. No suite preset runs it.

## Extract polygons

A dataset's `polygon.<name>` entries (`docs/brokkr.toml.datasets.md`) are
GeoJSON extract regions. `brokkr extract --polygon <name>` measures `pbfhogg
extract --polygon <file>` instead of the bbox (the two flags conflict), and
`brokkr multi-extract --polygon <name>` (repeatable, a name at most once)
appends `polygon_file` entries after the strips in the generated config,
naming each file by its absolute path; the config's polygon files are
declared reads of the launch, as the config itself is. Without the flag
neither command reads a polygon. The suite's extract bench runs its three
strategies on the dataset bbox and then on every dataset polygon, by name.
Every such row carries `polygon=<name>@<xxh128>` in its qualifier (`brokkr
man results`), so a changed file keys apart under the same name.

What a polygon selects is the build's. pbfhogg took `--polygon` from
b8bbf723 and `polygon_file` from 0192393a; from 59802597 a polygon node is
decided by osmium's integer rule, before it by pbfhogg's own point-in-polygon
test. The
argv is the same on both sides, so a `--compare` across 59802597 compares two
membership rules, and stored rows are not marked: a row's `commit` against
59802597 tells them apart. A polygon region has its own cost profile: pbfhogg
copies no node blob raw for it, and a config holding any polygon takes another
single-pass route than an all-bbox one.

## Check index

`brokkr check-index` measures `pbfhogg check --index` on the dataset PBF
like the other read-side commands (`--bench`, `--hotpath`, `--alloc`,
`--commit`, `--snapshot`), and is a `brokkr suite pbfhogg` preset. brokkr
passes `--index` alone, which suppresses pbfhogg's default `--ids --refs`
pair, so the row times index verification only. It decompresses every blob,
so it costs about a full `cat --clean` pass. It verifies the metadata a blob
claims, not that it claims any: absent indexdata or tagdata passes. A file
whose blob metadata disagrees with its payload exits 1 and is stored as a completed run (see
Answer exit codes). `--direct-io` is forwarded and changes every read the
command makes; a host that refuses O_DIRECT exits 3, a failure. The suite
preset runs without it. `--io-uring` is refused, as pbfhogg's check has no
io_uring path.

## Check locations

`brokkr check-locations` measures `pbfhogg check --locations`, which
verifies a LocationsOnWays file's inline way-node coordinates against its own
node records, with the same modes as its siblings (`--bench`, `--hotpath`,
`--alloc`, `--commit`, `--snapshot`). brokkr passes `--locations` alone, so
the row times that check only, and never drops it: without the selector
pbfhogg would run its default `--ids --refs` pair under this row's name, so a
`--commit` build from before pbfhogg 669a31e6, which lacks the flag, fails
instead. `--agreement` adds pbfhogg's `--agreement`, which also requires
every inline copy of an absent node's coordinate to agree; it needs a slot
per referenced node, so it is a different workload as well as a different
answer, and is declared separately (Declared answers).

pbfhogg refuses an input that does not declare LocationsOnWays, or declares
HistoricalInformation, before reading any data blob, exit 2. So
`check-locations` alone defaults `--variant` to an `add-locations-to-ways`
output: `altw-keep-untagged`, which
keeps every referenced node so a reference to an absent node is a failure;
with `--agreement`, `altw`, which drops most untagged nodes, the ones
`--agreement` holds to their other references. An explicit `--variant` wins,
and a dataset or snapshot without the default fails resolution with no
substitute.
Whether a file is checkable is left to the binary, which alone knows what its
revision accepts, so an inapplicable input fails after the build. It is not a
`brokkr suite pbfhogg` preset: the suite runs every preset on one selected
input, by default `indexed`, which is generally not LocationsOnWays.
`--direct-io`, `--io-uring`, `--compression` and `--jobs` are refused, since
no pbfhogg revision gives any of them effect there; for direct I/O, which its
unaligned parallel reads never take, pbfhogg printed a note until 930ddfc9
and refuses it since.

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
  `Command::into_route` (`src/command_input.rs`). The `read` throughput benchmark accepts `--snapshot` too (it resolves
  outside `build_pbfhogg_context` but share `resolve_snapshot_pbf_path` via
  `SnapshotRef::from_opt`); the synthetic `write`/`merge-bench`/etc. benches
  do not.
Promoting a command's output into the snapshot graph (`--as-snapshot`,
`--replace-snapshot`) is no longer a brokkr flag: `brokkr run --with pbfhogg
pbfhogg-datasets -- promote ...` does it from pbfhogg's own binary.

## I/O and compression flags

pbfhogg-specific flags that adjust cargo features and binary args. Note on
result rows: post-v13 the `mode` column (formerly `variant`) carries only the
measurement mode (`bench`/`hotpath`/`alloc`). These axis flags are **not**
folded into a variant suffix anymore - they are in the brokkr argv, so in
`brokkr_args` and the `arm_key` `brokkr results --compare` pairs on, and in the
child's `cli_args`; the `brokkr results` table's `args` column renders them.
So a flag-on run and its flag-off baseline at the same commit are distinct
rows / distinct compare pairs automatically, no suffix required.

**Which commands take them.** brokkr appends `--direct-io`, `--io-uring`,
`--compression` and `--jobs` after the rest of the argv, and refuses one, as
a usage error before anything is built or preflighted (dry run included),
only on a command shape where no pbfhogg revision gives it effect: the flag
was never accepted there, or it was a no-op until pbfhogg began refusing it
(6e87c0ef). Where some revision honours it, brokkr forwards it and the
revision built answers. The table lives in `PbfhoggCommand::axis_has_effect`
(`src/pbfhogg/commands.rs`):

- `--direct-io` - enable O_DIRECT I/O. Adds `linux-direct-io` cargo feature,
  `--direct-io` binary flag. Every command but `merge-changes`,
  `check-locations` and `tags-filter --input-kind osc`. `check-refs` keeps
  it: pbfhogg revisions from fb8dd3c0 until 053def6a read it. From 930ddfc9
  pbfhogg refuses it there with exit 64, so a run built from such a revision
  fails with that refusal.
- `--io-uring` - enable io_uring I/O. Adds `linux-io-uring` cargo feature,
  `--io-uring` binary flag. Runs io_uring preflight checks before building.
  Only `apply-changes`, `sort`, `cat --dedupe`, `repack` and `degrade`, the
  commands with an io_uring writer; `diff` never had the flag.
- `--compression <spec>` - output PBF compression passed through to the
  binary, so only commands that write a PBF: not `inspect`, the `check`
  commands, `digest`, `diff`, `merge-changes`, `build-geocode-index` or an
  OSC `tags-filter`. Values: `none`, `zlib` or `zlib:N` (0-9, every
  revision's range), `zstd` or `zstd:N` (any integer; the built revision
  checks the level, -7..22 since pbfhogg c8ee4531). Bare `zlib` and `zstd`
  use the binary's default levels. No cargo features required.
- `-j/--jobs` (on the subcommands that expose it) - `inspect --nodes` and
  `--tags` (the default inspect has no worker pool), a `tags-filter`
  without `-R` (`-R` has no pool it sizes; the OSC filter refused `-j`
  until pbfhogg 07c67afc), `diff`, `diff-snapshots`, `apply-changes` and
  `merge-changes`. `-j 0` counts as given.
- `tags-filter -R` with `--remove-tags` is refused too, since `-R` drops the
  referenced objects `--remove-tags` would strip; so are `-R`,
  `--invert-match` and `--remove-tags` with `--input-kind osc`.
- `extract` and `multi-extract` forward `--direct-io` and `--compression`
  (they used to drop both silently).
- The synthetic `read`, `write` and `merge` benches run pbfhogg's
  `bench-read`, `bench-write` and `bench-merge`, which take none of the
  three, and refuse them rather than ignore them: pick codecs with
  `--compressions`, and io_uring on `merge` with `--uring`. Each runs one
  cell per variant (`src/pbfhogg/bench_cells.rs`) on one build, which carries
  the measured mode's instrumentation feature, and with `--uring`
  `linux-io-uring` for every merge cell, buffered ones too, so the I/O mode
  is all a pair of cells varies. `--hotpath` and `--alloc` profile each cell
  like any other command; every mode records the wall the child reports, and
  each row carries its cell as a qualifier (`brokkr man results`).
  `--dry-run` prints every cell's argv.
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
  `brokkr sidecar --counters` unchanged.
- `--force-altw` - (`add-locations-to-ways` only) appends `--force` to the
  pbfhogg child (`src/pbfhogg/commands.rs`, `AddLocationsToWays` arm), skipping
  its indexdata requirement so raw / non-indexed input reaches the decode-all
  fallback path. Named to disambiguate from brokkr's own per-subcommand
  `--force` dirty-tree override, mirroring `--force-repack`. Composes with
  `--index-type sparse` or `auto`, `--compression`, and the measurement
  modes; pbfhogg refuses `--force` with `--inject-prepass` and with
  `--index-type external`, which need indexdata whatever `--force` says.
  brokkr does no validation of its own - pbfhogg owns the semantics. The
  intended cell: `brokkr add-locations-to-ways --dataset europe --variant raw
  --index-type sparse --compression zstd:1 --force-altw --bench 3`.
- `--force-sort` - (`sort` only) the same pass-through for `pbfhogg sort
  --force`, which an input without blob-level indexdata (a `pbf.raw`
  variant) needs: `brokkr sort --dataset europe --variant raw --force-sort
  --bench 3`. Named like `--force-altw` and `--force-repack`. `--force-altw`
  and `--force-sort` set one pass-through, `CommandParams.force_unindexed`;
  `--force-repack` rides repack's command variant.
- `--keep-untagged-nodes` - (`add-locations-to-ways` only) forwarded
  verbatim to the pbfhogg child, keeping untagged nodes in the output. It
  takes the pbfhogg name since brokkr has no flag it collides with. Composes
  with `--index-type`, `--force-altw` and the measurement modes; pbfhogg
  judges the combinations.
- `--type KIND` - (`diff` only) forwarded verbatim to `pbfhogg diff --type`;
  pbfhogg validates the value.
- These pass-throughs are brokkr flags, so they are in `brokkr_args` and the
  `arm_key` like every axis above: a cell with one pairs only with cells
  that have it, and a row stored before the flag existed keeps the key it
  had, since its argv never carried it.
- `--locations-on-ways` - (`apply-changes` only) passes through to the child
  pbfhogg invocation. It needs a
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
`pbf.locations` with a `brokkr env` xxhash.
