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
  Also `LibraryHarness`, the handle on the built library harness.
- `src/pbfhogg/library/` - `brokkr library`, pbfhogg used as a library
  through a harness a pbfhogg workspace member provides: listing its
  scenarios and verifying one, its calls' CLI counterparts replayed and
  compared by `pbfhogg digest`, measuring or profiling one, and certifying
  every scenario against `[pbfhogg.library.require]`; the command and its
  protocol are `brokkr man library`, its internals
  `reference/library-internals.md`.
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
verify check - and on nothing that only reads. The one exception is a
`brokkr library` CLI counterpart that declares its own `overwrite` stance
(`brokkr man library`, CLI equality): `true` requires the flag, `false`
never passes it.

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
that (an argv with outputs negotiates `--overwrite` unless it forces its own
or declares its stance, as a library counterpart may),
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
`PbfhoggBinary`, or, for the library harness (`brokkr man library`), as a
`LibraryHarness`, the same kind of handle made in the same module: it holds
the lock and the build registration, re-stats its executable before each
spawn and exposes no path. The executable the runner hands a plan is guarded by types:
its path needs a key only the handle can make, and pairing it with argv
directly is refused for a pbfhogg build. The handle and its invocations
expose no path, and reference tools are a closed set (osmium, osmconvert,
osmosis) launched by the absolute path their resolver produced. The remaining seams that hand out a
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
text `diff` to stdout, which only verify runs) in the root the argv names, the
verify directory. Before anything is
created, every output's directory must be on the same mount as that location,
or the launch is refused, since pbfhogg publishes by rename. A launch in a
`brokkr library` run directory (a CLI counterpart's replay and the digests
that compare it, `brokkr man library`, CLI equality) runs in a working
directory inside the run directory rather than the project root and
reserves the whole run directory as coverage; its private directory follows
the same rule, so it goes beside the replay's first output, inside the run
directory, and a digest, which writes nothing, gets none. Scratch is
declared by what pbfhogg's implementation actually does:
`add-locations-to-ways`, `build-geocode-index` and `diff --format osc` on
every path, and `merge-changes` only where it can spill: several inputs, no
`--simplify`, and a `--jobs` other than 1 (an omitted or 0 `--jobs` is auto
and counts, whatever it resolves to on the host). Since pbfhogg 2b1c6ba1 only
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
baseline accepts the long forms; the one exception is `verify renumber`'s
`diff -s`, whose long form was renamed while the short kept its meaning.
Rows recorded earlier carry short spellings in `cli_args`. The argv carries
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
Coverage is a tree reserved for writing around the launch's own paths: the
library harness's launch, and every CLI launch of the same run, covers the
whole run directory (`brokkr man library`), so the lease protects the
request and artefacts beside its outputs. It is reservation only: the private directory's placement compares
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
outlived its run, at a later scan. A profiled `brokkr library` harness launch
is the exception: its report lies in its run directory, which the launch's
coverage already reserves, so it survives the release to be judged and goes
with the directory (`brokkr man library`, Profiling a scenario).

**What the reservation protects.** Every brokkr deletion, rename or
replacement that can touch these paths - `brokkr clean`'s removals of
pbfhogg's verify output and scratch, a `brokkr library` run directory's
removal, a pbfhogg measured run's scratch dirs,
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
  by the registered digests of both files, so a rotation by `download
  --refresh` or a `--replace-snapshot` can never point a declaration at
  another file. A side without an `xxhash` cannot be addressed and stays
  undeclared.

The accepted exits are the answers the build can be trusted to give
intersected with the declared one. A build from before the split, asked to
answer `violations` or `differs`, leaves nothing to accept: the run is
refused before it starts. A declared run says what it expects before it
runs, and a dry run lists it. The suite reads the declarations of the
primary entry it measures.

`repack --as-snapshot` records one declaration itself: pbfhogg's repack
round-trips every element, so the new file and the entry it read are
recorded as `same` under `diff-snapshots` when the source has a registered
digest. `cat` and `degrade` record nothing: `--type` drops kinds, `--clean`
drops versions, `--strip-locations` drops way coordinates, and whether
`--unsort` leaves an ID-order violation depends on the input's size. No
check verdict is ever derived: a repack can repair the metadata its input
lied about.

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
output: `altw-keep-untagged`, the name `verify all` also looks for, which
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

## Verify subcommands

`brokkr verify` holds the brokkr command lock for the whole invocation,
including `verify all` and its per-check runs, and a `SigtermGuard` alongside
it, so `brokkr kill` or Ctrl-C ends the running pbfhogg/osmium child, prints
`<name>: INTERRUPTED`, stops `verify all` at that check (no further checks, no
tally banner) and exits 130.

**Output model.** Every verify check runs "quiet on pass, loud on fail" via
`verify::run_check`: by default a check's detail (`verify_msg` output - section
headers, inspect dumps, element diffs) is captured into a buffer and only
replayed if the check fails; a passing check prints just a one-line
`<name>: PASS (<ms>ms; <steps>)` summary. `<steps>` is the check's wall time
per tool run label, slowest first, with a run count when a label ran more
than once (`osmium sort 2m0s, pbfhogg sort 41.2s, pbfhogg diff 15.1s (2
runs)`), so the summary says how a check's time splits between the pbfhogg
command under test, the reference tool and the comparisons; a reference step
served from the reference cache is listed last as `osmium sort cached`, with
no duration. A passing check adds at most one informational line of its own on
the summary channel, naming the file that holds the detail. INTERRUPTED and
FAIL lines carry the same breakdown. While a check runs on a terminal, a live
status line names the step in flight (the same activity `brokkr lock` shows),
so a long osmium step is not silent. `-v`/`--verbose` skips the buffer so
detail streams live even on pass. `verify all` applies this per check, so its default
output is one line per check plus a final tally, and only failing checks spew.
The buffer lives in `output.rs` (`verify_buffer_begin/flush/discard`, fed by
`verify_msg`); one-line results use `verify_summary`, which bypasses it. On
failure `run_check` returns `DevError::ExitCode(1)` so `main` exits non-zero
without re-printing an error it already reported.

**Environment outcomes.** A reference tool (osmium, osmconvert, osmosis)
killed by `SIGKILL` is not a pbfhogg failure: brokkr never sends a reference
tool SIGKILL (its own stop is a SIGTERM that ends as INTERRUPTED), so the
kill came from outside, commonly the kernel OOM killer on a host too small
for the reference tool at that scale. osmosis's start script `exec`s java,
so an OOM-killed JVM ends as that signal too, while a Java
`OutOfMemoryError` is an ordinary non-zero exit. `VerifyHarness::reference`
raises it as `DevError::VerifyEnvironment`, classified from the child's ending before any
capture failure, and the check prints `<name>: ENVIRONMENT (<ms>; <steps>):
<reason>` with its detail replayed. A comparison failure recorded earlier in
the same check still wins: `Findings::fail` also writes a check-scoped
ledger that an early `?` cannot drop, so the check is a FAIL naming both.
Only `SIGKILL` of a reference tool is environmental; another signal, a
non-zero exit, or pbfhogg itself being killed (its memory at scale is under
test) stays a FAIL. A single check exits 75 (`EX_TEMPFAIL`) on ENVIRONMENT;
`verify all` tallies `environment` beside passed, failed and skipped and
exits 1 if any check failed, else 75 if any was environmental. An
environment kill of osmosis or osmconvert, which `verify merge` runs for
context only, is reported on the summary channel and changes no verdict.

**Output that scales with the data.** No verify step holds a listing in
memory. Every reference-tool run, and every pbfhogg step whose stdout is a
listing (`diff_pbfs`, renumber's `diff -s -c -v`, `verify diff`'s text
diffs), writes stdout to a file in the check's directory
(`Invocation::run_to_file`, `output::run_stdout_to_file`); stderr stays a
bounded capture. Such a child dies with brokkr (`PR_SET_PDEATHSIG`, refused at
spawn if it cannot be armed), since a file, unlike a pipe, gives an orphaned
writer no reason to stop. The signal reaches the process brokkr spawned; a
launcher that `exec`s its program passes it on, while helpers a launcher
forks first (osmosis's start-script preamble) are left to the orphan
reaper. Consumers read the files through fixed buffers
(`verify_stream`): per-line heads for renumber's block count, emptiness for
`diff_pbfs`'s verdict; `verify diff` reads its listings and osmium's OPL record
by record, holding one identity's objects and detail at a time. A FAIL replays a bounded head of a listing (lines and bytes per
line) and always names the file holding all of it. Every `inspect` verify
reads - element counts, the header's sort flag, the computed order
(`check_sorted`) and the summary `print_inspect` shows - is `inspect --json`
(with `--extended` where the order or the extended summary is needed)
written to a file beside the inspected output and read by a streaming
parser that keeps only those fields and skips every other value as it
streams, without collecting its contents. The file stays as the full report and the
printed summary names it. An extended report without `data.objects_ordered`
is an error, never read as unordered. `check --refs` counts stay captured;
every report consumer still requires its documented exit codes before
trusting what it parsed. (pbfhogg itself builds the whole report as
one JSON value in memory before writing it.)
`verify merge`'s element diff OSC is classified as it streams into per-class
counts and the ten smallest IDs; the one structure it holds is the input
OSC's delete set, O(delete entries in the change file), which the osmium
apply-changes step before it dominates by holding every change as a full
object. `verify extract`'s simple relation judgement holds the node and way
IDs osmium's complete_ways relations reference, the subset of them present,
and the IDs of the relations each diff lists, all sorted vectors; the OPL
relation records stream past twice. `verify getid-removeid`'s
`--add-referenced` arm holds, from the input's relation records (streamed
twice), only the relations that hold relations with their relation members,
and the objects of osmium's `getid -r` output, one closure. `verify diff` holds the identities to fetch:
those its input OSC touches and those pbfhogg reports modified, as a sorted
vector, O(change set), and the identities of osmium's listing, O(differences).
`verify check-ids` reads each `check --ids --json` report from its file,
keeping the counts and the verdict and counting the violations, which
pbfhogg bounds by its `--max-errors` default; osmium's `fileinfo` report is
read the same way. `verify getparents` holds the objects of one osmium
output at a time, outputs of a handful of requested IDs, and from the
input's relations only the first member of each kind.

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
code) is an error - never "identical". A check whose PASS establishes less
than its name suggests builds its findings with `Findings::claiming` (or
states it once known with `Findings::with_claim`, where what it established
depends on the input), and the claim is printed in its PASS line, under
`verify all` too.

`extract` and `verify merge` classify a `pbfhogg diff --format osc` between
pbfhogg's and osmium's outputs as it streams (`verify_element_diff`); the
diff's exit status must agree with the OSC (0 with no entries, 1 with some),
or it is an error; the generated OSC is read strictly, so an entry without a
parsable ID, an entry outside an action block or inside another entry, a
nested action block and an end tag closing nothing open are errors rather than
skipped. `extract` fails on a `<modify>`:
an ID both tools emit with unequal content, which no strategy explains, since
neither tool rewrites an element it extracts.

`extract` also gates membership, per pbfhogg's stated per-strategy contract
against osmium (its DEVIATIONS.md, "extract: per-strategy membership against
osmium"). complete_ways must hold osmium's element set, and smart the set of
`osmium extract -s smart -S types=multipolygon,boundary` (the `-S` is in the
reference argv, so in the cache key): any element only one tool holds is a
FAIL. simple must hold osmium's nodes and ways and every relation osmium's
simple holds; its relations differ by design, since osmium's simple decides a
relation on its first node or way member and pbfhogg's on any direct one. They
are judged against osmium's complete_ways output, which holds every relation
simple's node and way sets match plus their parents
(`verify_extract_simple`): its relations go to OPL (`osmium cat -t relation
-f opl`), read by a strict reader (`verify_opl`) whose record count must equal
inspect's; `osmium removeid` of every node and way they reference, diffed
against osmium's simple output, names the members simple's sets hold; and a
second element diff from pbfhogg's simple output to osmium's complete_ways
output must list nothing only simple holds and no content difference, which
checks the content of the relations osmium's simple omits too. Then every
complete_ways relation must be in pbfhogg's simple output exactly when one of
its direct node or way members is in those sets, and every relation only
pbfhogg's simple holds must have its first node or way member outside them.
That judgement runs only when simple's node and way membership agrees with
osmium's, which a disagreement already fails. A negative node or way member
reference is an error, not a verdict: `osmium removeid` cannot look one up.

Both outputs go through the strict order check; an osmium output that fails
it is an error, since a repeated ID could hide a content conflict from a by-ID
comparison. The detail gets, per strategy, the shared coverage per kind (given
only when pbfhogg's output is strictly ordered too, so every count is of
distinct IDs, and an error when the arithmetic does not close) and the
pbfhogg-only and osmium-only counts with their smallest IDs. The summary
channel gets one line naming, per strategy that differs, the nonzero
pbfhogg-only and osmium-only counts, and the strategies' diff OSCs
(`pbfhogg-vs-osmium-<strategy>.osc`), which list every element; on a pass its
only clause is simple's pbfhogg-only relations. A PASS line says `output
order, element content and membership against osmium: complete_ways and smart
(-S types=multipolygon,boundary) as osmium's, simple as osmium's but for the
relations osmium's first-member rule omits`, followed by the regions it ran
on.

`extract` runs every strategy on each region: the bbox (`--bbox`, else the
dataset's) in `target/verify/extract/`, then every dataset polygon, by name,
in `polygon-<name>/` below it, pbfhogg with `--polygon <file>` and osmium with
`-p <file>`, the file an input of the reference run and so keyed by its
content and extension. Each pass takes every gate above unchanged, and its
failures, detail sections and summary line name the polygon. With polygons
configured the bbox is optional; without either the check is a configuration
error. The PASS line ends `on the bbox and on polygons <names>`, or names the
kind it did not exercise: `on the bbox only: no dataset polygon is
configured, ...` or `on polygons <names> only: no bbox is given or
configured, ...`. Before any run, each polygon file must be one osmium reads
as pbfhogg does (`verify_polygon`), since pbfhogg's DEVIATIONS.md ("extract:
polygon input beyond osmium") names three readings where the tools select
different regions by design: a FeatureCollection of several features
(pbfhogg unions them, osmium takes the first), an open ring (pbfhogg closes
it), and a ring with an edge spanning more than 180 degrees of longitude
(pbfhogg unwraps it across the antimeridian). The gate accepts a bare Polygon
or MultiPolygon, a Feature holding one, or a FeatureCollection of exactly one
Feature, every ring closed and no edge, the closing one included, spanning
more than 180 degrees, judged on coordinates parsed correctly rounded as
pbfhogg parses them; anything else refuses the check as cannot-compare. What
a tool refuses outright (a zero-area ring, a coordinate outside WGS84, a third
coordinate for osmium, a file osmium does not read as GeoJSON by its `.json`
or `.geojson` extension) fails as that tool's error.

`extract` branches on `Sort.Type_then_ID` among the input header's optional
features, where pbfhogg's extract reads it. On an input without it (a
`degrade --unsort` snapshot), osmium,
which refuses out-of-order input, extracts from an osmium-sorted copy (the
run and artifact name `verify sort` uses, so one cached sort serves both),
and pbfhogg extracts the input as it is, through its unsorted fallbacks. Its
order verdict becomes its header contract: an output that claims
`Sort.Type_then_ID` (in either feature list) fails. Each pbfhogg output is
then compared through an osmium-sorted copy, whose strict order proves it
repeats no ID; osmium's outputs, read from a sorted copy, are compared as
they are. The input
header's report goes to the check's directory, never beside the input. Which
order pbfhogg keeps an unsorted input's elements in is not promised and not
checked. The PASS line then opens `on unsorted input, against osmium on an
osmium-sorted copy: no sorted claim and no repeated ID in pbfhogg's output`.

`multi-extract` holds each region
of a simple `--config` run to strict order and diffs it (not its counts)
against a sequential pbfhogg extract of the region; it never consults
osmium's extract membership (on unsorted input osmium only sorts the
comparison copies) and does not prove the single-pass path ran. It runs two configs: the strips
alone, and, when the dataset configures polygons, the strips followed by
every polygon as a `polygon_file` entry (in `mixed/`), since pbfhogg's single
pass takes another route once any region is a polygon. Both compare against
the same sequential extracts, one per region. Its PASS line names the configs
it ran, or that no mixed config ran for lack of a polygon. On an input
without `Sort.Type_then_ID` among its optional features pbfhogg declines its
single pass, so no single-pass route is exercised, and the PASS line opens
saying so: every multi and sequential output must then not claim the flag,
and each is compared through an osmium-sorted copy whose strict order proves
it repeats no ID.

`cat` and `tags-filter` take any order and keep it, and branch the same way
(`verify_input_order`): on sorted input each pbfhogg output must be in strict
order and its sort declaration is compared with osmium's; on an input
without the claim osmium reads the osmium-sorted copy, each pbfhogg output
must not claim the flag and is compared through an osmium-sorted copy, and
the PASS line opens `on unsorted input`. On either, an osmium output that is
not in strict order is an error. `check-refs` has no output to compare:
pbfhogg's `check --refs` reads the input as it is, `osmium check-refs`
(which refuses out-of-order input) the osmium-sorted copy, whose elements are
the input's, and the PASS line says so. Its comparison is the missing count
per class (nodes in ways, and with `--check-relations` also nodes, ways and
relations in relations), and each tool's exit must agree with its report:
exit 1 exactly when something is missing. pbfhogg's report must carry one
integrity verdict, `OK` with exit 0 or `FAILED` naming a positive count with
exit 1, or the check fails; an `OK` beside a class line or a `FAILED` total
its class lines do not account for leaves no count to compare, a FAIL too. osmium prints its summary to stderr only after a
whole read and also exits 1 when an exception ends its run (`Node IDs out of
order`), so its exit 1 completes the run only on a complete summary for its
mode as the last lines of stderr; a summary that is missing, ambiguous or
contradicts its exit, or anything on its stdout, is an error.

`merge`, `derive-changes`, `diff` and `renumber` verify commands whose
documented contract refuses input without `Sort.Type_then_ID` (pbfhogg's
`apply-changes` takes a sorted base, `diff` in both formats needs both inputs
to claim it, `renumber` requires sorted input), and osmium's
`apply-changes` and `renumber` require sorted input too, so on such input
(read from the header's optional features, as the checks above read it)
nothing can be compared and the check is held to that refusal instead
(`verify_input_order`). The command under test runs on the input with the
argv the check would give it (`--direct-io`, `--locations-on-ways` and
`--start-id` included): an answering exit (0, or 1 for a diff) fails the check
as acceptance of input pbfhogg refuses, which its general apply-changes path
once did, silently dropping creates; so does an output under any name the
argv writes, or a text diff listing on stdout, whatever the run's ending. Exit
2 must carry exactly one of the `pbfhogg sort --distrust-metadata <input> -o
sorted.osm.pbf` advice lines pbfhogg's CLI gives every `Unsorted` refusal,
its shell word decoded and equal to the claimless operand as the argv spelled
it (one naming another file is a failed finding); exit 2 without it, another
exit or a signal is an error, a failure of another kind. `merge` and
`renumber` run once; `apply-changes` itself is the step that refuses, so
`derive-changes` and `diff` never build their new PBF and instead run the
command under test twice, the input as OLD and then as NEW against an
osmium-sorted copy (verify sort's cached run) that must itself declare the
flag, each with an output of its own. A mixed indexed pair, so neither diff
engine is exercised. Under `--locations-on-ways` the base must declare
`LocationsOnWays`, or the check is an error before apply-changes runs, since pbfhogg
refuses that too and the check would otherwise depend on which refusal it
raises first. No osmium run is compared. The PASS line opens `on input without
Sort.Type_then_ID among its optional features, refusal only` and says what it
does not establish: which of pbfhogg's order checks refused (the header gate,
or a later one meeting the content out of order), and under `--direct-io`
that any O_DIRECT read ran. A base that claims the flag over disordered
content takes the ordinary path, where pbfhogg's refusal of it is an error.

**Fresh outputs.** `VerifyHarness::subdir` empties `target/verify/<check>/`
before handing it back, so every file a check finds there was written or
placed by this run. Checks rely on that: `verify_merge` treats the diff OSC's
existence as proof the diff ran. The optional osmosis/osmconvert outputs are
printed only for a tool whose run succeeded (tracked from the run, not from
the file existing), and a failed optional tool's partial output is removed.

**Reference cache.** Every reference run (osmium, osmosis, osmconvert) is a
declared `ToolRun` - literal arguments, inputs and outputs kept apart - run
through `VerifyHarness::reference` (`src/pbfhogg/verify_reference.rs`), which
reuses a completed execution of the same run from `.brokkr/verify-reference/`.
Only the tool's execution is skipped: every pbfhogg step (inspect, diff, sort
normalisation, `check --locations`, the element diffs) still runs, against the
reused reference bytes, and no pbfhogg observation or verdict is cached. The
contract is reuse of a completed execution of the same declared computation,
with its artifacts and its exit code; it does not claim a rerun would write the
same bytes. A hit shows as `<tool> <command> cached` among the step times
and, in the detail, the entry's key and stored exit.

- **Key**: the run id, the tool's identity, the controlled environment, each
  input's content digest and file-name extension (taken from the resolved
  path, which is what the launch passes), and the argv with inputs and outputs
  as placeholders. Identity is osmium's or osmconvert's resolved executable,
  osmium's `--version` and every shared library `ldd` resolves for it; for
  osmosis, its install tree, its Java runtime tree and the libraries `ldd`
  resolves for `java` and `libjvm.so`. The probes run in the same controlled
  environment as the tool. A program whose identity cannot be established runs
  the same way but is never cached, said once per program.
- **Launch**: every reference run, cached or not, runs in a fresh staging
  directory with no inherited user environment - `PATH=/usr/bin:/bin`,
  `C.UTF-8`, `TZ=UTC`, `HOME` and `TMPDIR` inside staging, and for osmosis
  `JAVA_HOME` plus `JAVA_OPTS` setting `user.home` and `java.io.tmpdir` inside
  staging, since osmosis looks for plugins under `user.home` - beside brokkr's
  own lifecycle variables. Its inputs are reserved for reading and staging for
  writing. Where the staging path holds characters the osmosis launcher splits
  (whitespace, quotes, backslashes), osmosis runs without those two properties
  and is never cached.
- **Publication**: only a run that exited with an accepted code (0, or 1 for
  `osmium diff`; 1 for `osmium getid -r` and `check-refs` only when its
  stderr shows the run completed, `ToolRun::accept_if`), wrote every declared
  output, and whose key
  is unchanged when recomputed after the run is published: artifacts made
  read-only, hashed and stamped, a manifest written, everything synced, and the
  staging directory renamed into place, an older entry renamed aside first. A
  signalled, interrupted or failed run is never stored. A storage failure is a
  warning; the run's result is still used.
- **Placement**: a check never receives a cache path. Each artifact is
  hard-linked into the check's own directory under a name that must not exist
  (copied through a temporary name across filesystems), so the check owns its
  names as it owns a fresh output, inspect reports land beside them, and
  removing or replacing an entry never takes a file from a live check. Nothing
  writes a reference artifact in place.
- **Integrity**: a lookup rechecks the stored exit against today's accepted
  codes and the artifact set against the run's, and compares each artifact's
  stamp (device, inode, size, mtime); a moved stamp is rehashed against the
  digest recorded at publication; a stored exit accepted only on a stderr
  condition is held to it again on the stored stderr. ctime is not in the
  stamp, since placing a link moves it. This catches a replaced or rewritten artifact whose stamp
  moved, not an mtime-preserving rewrite or silent media corruption. An
  invalid entry is removed and the run is a miss.
- **Outside the identity**, so able to change a run without a miss: system
  utilities a launcher script runs from the fixed `PATH`, libraries loaded at
  run time outside the `ldd` closure, symlink referents inside a hashed tree,
  and an input or tool changed while a run is in flight by something other than
  a cooperating brokkr process.
- `brokkr verify --fresh-reference` runs every reference tool for real and
  replaces its entry. `brokkr clean` removes the whole cache, and `brokkr
  clean --verify-reference` removes it and nothing else. A host's
  `verify_reference_budget` (`brokkr man config verify_reference_budget`)
  bounds it: when each verify ends, the most recently used entries that fit
  stay, a hit refreshing an entry's recency through its manifest's mtime.
  Without the key nothing evicts entries.

**`verify all` inputs.** The OSC- and bbox-consuming checks skip only when
the input is not configured at all (no `--osc-seq` and no OSC registered for
the dataset or named snapshot; no `--bbox` and no dataset `bbox`, and for
`extract` no dataset polygon either; `multi-extract` cuts its strips from the
bbox, so it skips without one). A configured input that fails to resolve -
hash mismatch, missing file, several OSCs and no `--osc-seq`, a malformed
bbox, a polygon file that is missing or fails its pin - fails the command
before the suite runs rather than turning its checks into skips. osmosis
setup failure is non-fatal but narrated.

Every verify subcommand that takes `--dataset` also accepts `--input <PATH>`
to skip dataset resolution and use a handcrafted fixture, and `--snapshot
<key>` to cross-validate a registered snapshot (e.g. an adversarial encoding
from `degrade`/`repack --as-snapshot`) instead of the primary data. `--input`
and `--snapshot` are mutually exclusive. PBF resolution is centralised in
`resolve_verify_input` (`src/pbfhogg/cmd.rs`), which routes through
`resolve_snapshot_pbf_path` for both the base and named cases.

**`--direct-io`.** Verify defines one operation set and `--commit` only picks
the binary that runs it, so a verify subcommand offers `--direct-io` only
where the current pbfhogg takes it on the step it is forwarded to. It
reaches every run of the command a check verifies, and nothing else: `sort`,
`cat`, `extract`, `multi-extract` (the single-pass runs), `tags-filter`,
`getid-removeid` (every pbfhogg getid run), `getparents` (every pbfhogg
getparents run), `add-locations-to-ways` (each
engine), `check-ids` (every `check --ids` run, plain and `--full`), `merge`
(`apply-changes`), `derive-changes` (`diff --format osc`,
on the indexed pair and on the pair stripped of indexdata), `renumber`
(`renumber`) and `diff` (the text diff, indexed and stripped, plain and
`--verbose`). The steps a
verdict is judged against run buffered, so an O_DIRECT fault cannot sit on
both sides of a comparison or hand both tools the same wrong input: fixture
builders (the `apply-changes` that makes `derive-changes`' and `diff`'s new
PBF, `degrade --strip-indexdata`, `check-ids`' `degrade --unsort` and
`--unsort-intra` copies and its `cat` fixtures), references (`multi-extract`'s sequential
single-region extracts, `derive-changes`' roundtrip `apply-changes`) and
comparators (inspections, normalising sorts, element diffs, `renumber`'s
comparison `diff`, `check --locations`). `verify merge` and `verify extract`
hold `apply-changes` and the single-region extract to osmium under the flag.
`check-refs` does not take the flag, so passing it there is a parse error,
and `verify all --direct-io` runs it buffered: pbfhogg refuses
`--direct-io` under `check --refs` alone (exit 64, since 930ddfc9), where
its ref check reads through buffered parallel unaligned preads. Revisions
from fb8dd3c0 until 053def6a did read it there; O_DIRECT oracle coverage of
their ref check is given up rather than reached by probing the binary or
retrying a refusal.
Verify builds pbfhogg with the host's configured features and does not enable
`linux-direct-io` itself.

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

`verify_merge` reads the input OSC's delete set via `osc::scan_osc_file` and
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
error). osmium's output gets the same check, reported only: one summary line,
consistent or STALE with the report's mismatch count, so a change in what
osmium gets wrong shows from run to run, naming `osmium-locations.txt` in the
check's directory, which holds the whole report (the detail carries it too):
its location index is searched before it is sorted and resolves
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
compared), and `verify_diff` requires the indexed verbose listing byte for
byte (see below). The indexed answer is the one held to osmium, so the match
holds the unindexed path to osmium too. Both treat exit 1 from `pbfhogg
diff`, text or OSC, as "the inputs differ", and 2 and above as a failed run.

`verify diff` holds pbfhogg's text diff, every line of it, to osmium's
reading of the pair (the input and pbfhogg `apply-changes` of it with the
OSC). pbfhogg `diff --suppress-common` runs on the indexed pair and on the
stripped pair, each with and without `--verbose`. The expected listing comes
from osmium's content, never from pbfhogg: `osmium diff --suppress-common
--output-format opl` gives, with full content, every one-sided object and
every pair whose version, timestamp or osmium checksum differs. osmium's
checksum feeds tags without lengths, so a pair whose content differs can be
"same" to it (`a=bc` against `ab=c`, or a replayed equal-version change that
reorders tags), and it withholds that pair; every identity the input OSC
touches (read with the strict OSC scanner), and every one pbfhogg reports
modified, that the listing lacks is fetched from both inputs with `osmium
getid --output-format opl` (exit 1 accepted only on its completed verbose
output, the not-found count reconciled with what came back), and an
identity found on one side only is an error. Content is pbfhogg's definition
(node location and tags, way tags and refs, relation tags and members, in
order; metadata never), so a version or metadata change alone is no
difference. The indexed verbose listing must then hold exactly one record
per identity whose content differs, in object order, rendered as pbfhogg's
CLI reference documents: the header's class, type, ID and versions, and for a
modification each detail component (a node's coordinates, a way's ref and a
relation's member edit script, the tags keyed or, with a duplicate key, as
a script) accounting for its whole change and printing nothing when
unchanged, its literals and coordinates compared as the documented spelling
of osmium's decoded values. An edit script must be faithful and shortest
(the edit distance is recomputed) within 1000 edits, and a replacement must
name exactly the ranges left after trimming the common prefix and suffix.
The stripped verbose listing must equal it byte for byte, each plain
listing must equal its header lines, and every run must exit 1 exactly when
osmium's content shows a difference and summarise the created, modified and
deleted counts it gives and a common count of osmium's `same`, less the
fetched pairs whose content differs, plus the listed pairs whose content is
equal. osmium's own summary must match its listing's line counts.

What it does not prove: that `apply-changes` left the diff content of every
identity the OSC does not touch alone (an untouched object it changed while
keeping osmium's checksum, which pbfhogg's diff then wrongly called common,
would pass; `verify merge` holds `apply-changes` to osmium); which of
several shortest scripts pbfhogg prints; and whether a replacement within the
1000-edit cap was due to pbfhogg's search budget. A version 0 in osmium's OPL
cannot say whether a version is present, so either spelling is accepted;
the PASS line says so when one was seen, and names the renderings no record
exercised and any replacement accepted on the budget. A changed identity
with a negative ID that must be fetched, and a member pbfhogg prints with an
unknown type, cannot be compared and are errors.

`verify renumber` requires the comparison diff to exit 0 or 1 and its `-s`
summary (on stderr) to carry all four of `left`, `right`, `same` and
`different`; `same + different` must equal both totals, so equal totals
cannot hide one-sided elements, and the block headers classified by kind in
the listing must number exactly `different` (pbfhogg writes one per modified
element), so a listing the classifier does not fully read fails rather than
passing node or way differences as tolerated relation ones.

`verify getid-removeid` chooses its IDs from the input, not from a fixed
list. The include and `--invert` IDs come from a header-only walk of the
PBF's blob framing (`pbf_frames`), which reads each `BlobHeader` and its
indexdata and seeks past every data payload, so that choice costs the blob
count, never the data, at any scale (the `--add-referenced` request below
also reads the input's relations). For each kind
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
same walk. The input must not change on disk while the check runs. Its domain
is a snapshot with one record per kind and ID and true indexdata claims;
repeated records in a file that does not declare history are not looked for
in the input, but every output the check diffs, pbfhogg's and osmium's, must
pass the strict order check (an osmium output that fails it is an error), so
a repeat either tool wrote is caught. It branches on `Sort.Type_then_ID` as
`extract` does: on unsorted input every osmium run reads the osmium-sorted
copy `verify sort` shares, each pbfhogg output it diffs must not claim the
flag and is compared through an osmium-sorted copy (the `--invert` output is
only counted), and the PASS line opens `on unsorted input`.

The check then compares `getid --add-referenced` with `osmium getid -r`, and
`--add-referenced --remove-tags` with `-r -t` (`verify_getid_refs`), content
and tags compared by the same diff. The request is its own: one relation, the
highest claimed way and the highest claimed node. The relation comes from the
input's relations as OPL (`osmium cat -t relation -f opl` of the reference
input, its record count held to the indexdata claim), chosen so recursion
shows in the output: it holds a relation that holds relations, one of which
is present in the input and is neither the requested relation nor its direct
member, so that object is in the closure only through two hops. The lowest
such relation whose members include both nodes and ways is taken, else the
lowest including one of them, else the lowest such; without one, the lowest
relation by the same preference. What the run exercised is read from osmium's `-r` output as
OPL, never pbfhogg's: the two-hop relation, a pulled-in node and a pulled-in
way member of a relation, a node only a way pulls in, a tagged pulled-in
object (which `-t` must strip) and a tagged requested one (which it must
keep). The PASS line names each the input did not offer. osmium exits 1
whenever a requested or pulled-in object is missing, the normal case on an
extract and a documented pbfhogg deviation, and also when an exception ends
its run; it runs with `-v`, and exit 1 completes it only when the verbose
output reports `Did not find N object(s).` followed by the final `Done.`,
which the reference cache requires too before storing or replaying the run.
An input whose indexdata claims a negative ID is refused, and so is a
negative node or member reference in any object of osmium's output: osmium
folds a negative ID to its absolute value and pbfhogg does not, and every
reference either tool follows belongs to an object osmium wrote.

`verify getparents` compares `pbfhogg getparents` with `osmium getparents`,
each request with and without pbfhogg's `--add-self` (osmium's `-s`), by
`compare_outputs` as getid is: osmium reads the reference input (the
osmium-sorted copy on input without `Sort.Type_then_ID`) and pbfhogg the
input. The requests are one per element kind, since what a request holds
decides which blob kinds pbfhogg reads (a node request needs way and
relation blobs, a way or relation request relation blobs, and `--add-self`
adds the requested kind's own), so each exercises its own combination, and
one of every kind: W is the lowest and
highest way and the first way member of the lowest relation holding one, R
the same for relations, N the lowest and highest node, the first node member
of the lowest relation holding one and the first ref of a requested way (read
from osmium's W `-s` output), and M all of them. Lowest and highest come from
`osmium fileinfo -e` of the reference input, which must also show no ID
repeated, since the outputs are compared by ID, and no negative ID. Members
come from the input's relations as OPL, the run `verify getid-removeid`
reads, so one cached execution serves both. A member or ref need not exist:
a parent is found by its reference. osmium matches a reference by its
absolute value and pbfhogg does not, so every osmium output is read as OPL
before any comparison records a verdict, and a negative way ref or relation
member in any of them is an error: that is where every match its folding
adds would show. The same reading gives what the run exercised, never from
pbfhogg's output: a way parent of a requested node, a relation parent of a
requested node, way and relation, and per kind a requested object only
`--add-self` selects. The PASS line names each one the input did not offer.
A pass establishes selection, element content and tags compared by ID; not
metadata, the header, which blobs either tool skipped, or which of pbfhogg's
scan arms ran (the full-scan arm is chosen only from 150000 estimated blobs
up, so a regional dataset runs the header-walker arm).

`verify check-ids` runs `pbfhogg check --ids` and `check --ids --full`, both
`--json` with the report written to the check's directory. osmium has no ID
check, so the expectation comes from two readings of each file: `osmium
fileinfo -e -j --no-crc` (`verify_fileinfo`), whose `objects_ordered` and
`multiple_versions` say whether the decoded objects strictly ascend by kind
and then ID in canonical order (libosmium's `id_order` is pbfhogg's), and
brokkr's header-only reading of the framing (`pbf_frames::framing`), since on
an indexed file pbfhogg judges type order from the kind every data blob
claims, an empty blob's included. A file passes when osmium reads it in
strict order and, if indexed, its claims never step back a kind. An ordered
report without `multiple_versions` is an error, never read as false. Each
mode must exit as its report says (0 with a pass and no violation, 1 with a
failure and at least one), answer as expected, count each kind as osmium
does (what catches a reader that skips objects), and report `indexed`
exactly when the framing carries indexdata. The violations themselves and
`--full`'s duplicate detail are not compared, and the PASS line says so. The
cases: the input; when osmium reads it in strict order, three copies the
check derives, the input stripped of indexdata (when it has any), `degrade
--unsort --elements-per-blob 8000` (a step down across blobs, indexed) and
`degrade --unsort-intra --strip-indexdata` (one inside a blob, unindexed),
each required to keep the input's counts, its framing, and to be out of
order exactly when its degradation fires on those counts (a kind above the
cap, a kind with two objects), else the check is an error; and five fixed
fixtures, written as OPL and converted by `osmium cat`, then rewritten by
`pbfhogg cat` for an indexed form: a way before its nodes and an adjacent
repeated ID in both forms, and an ordered file with an empty blob claiming
nodes appended after its way blob, which osmium reads as ordered and pbfhogg
must fail. Each fixture must show the property it exists for, read from
osmium and the framing. The PASS line names each kind of case, indexed or
not and in order or not, that neither the input nor a copy derived from it
gave (an input osmium reads out of order derives nothing, and neither does
one with no object); the fixtures do not count toward it. An input with
a negative ID is refused (pbfhogg rejects negative input IDs project-wide,
and `--full` reports each as untrackable), and one whose framing mixes blobs
with and without indexdata too. Kind claims must be true: a blob whose
decoded kind contradicts its claim stops pbfhogg's check without an answer,
an error here.

`verify add-locations-to-ways` runs osmium with `--keep-member-nodes`: pbfhogg
keeps untagged relation-member nodes unconditionally (a documented pbfhogg
deviation), osmium only with that flag.

It verifies pbfhogg's node-index engines by name, each run with an explicit
`--index-type`: `--mode sparse`, `--mode external`, or `--mode all` (the
default, and what `verify all` runs) for both. `auto` is not verified on its
own, since it routes to one of the two and its output is that engine's. Every
engine runs before any check reads an output.

The input header's `Sort.Type_then_ID` claim (among its optional features)
picks each engine's route (`verify_input_order`). Sparse takes any order: on
input without the claim osmium reads the osmium-sorted copy, and sparse's
output must not claim the flag and is compared through an osmium-sorted copy,
whose strict order proves it repeats no ID (the copy keeps no inline
location, which the diff does not read). External's documented contract
refuses such input, so there it is held to that refusal as `merge` is (exit
2, the one `Unsorted` advice naming the input, nothing published) and never
compared: under `--mode external` osmium does not run and the PASS line opens
`on input without Sort.Type_then_ID among its optional features, refusal
only`; under `all` the PASS line names sparse's comparison, external's
refusal and that the engines are not compared with each other. A header that
claims the flag over disordered content takes the ordinary route.

On the ordinary route an engine that exits non-zero is a recorded failure
with its stderr in the detail, never a skip, and brokkr does not re-derive
why: the domain is the input both engines and the checks accept - indexed, no
negative node IDs or references, no `HistoricalInformation` header - and an
input outside it fails the check without that alleging an engine defect
(verify an input external refuses with `--mode sparse`). Each output that was
produced gets:

- the comparison with osmium's output: strict order and the sort-declaration
  comparison on sorted input, the header and repeated-ID checks above on
  input without the claim, then the element diff;
- `pbfhogg check --locations --agreement`. `pbfhogg diff` treats inline
  coordinates as derived data, not content, so the diff never sees one; this
  holds each coordinate to the node record the output keeps and, for a node
  it dropped (most untagged nodes), to the other references to that node. It
  runs on the output itself, never a sorted copy.

Under `all`, when both engines produced an output, the two outputs' node, way and relation lines of `pbfhogg digest
--form full`, which cover metadata and exact way coordinates, must be
identical; the overall line is ignored, since it also covers the header. A
digest that fails or prints a malformed report, or two reports of different
form versions, is an error. The PASS line carries the claim: inline locations
are never compared with osmium's, so a wrong coordinate for a dropped node
that every reference to it shares, written by both engines alike, passes.
Under `--commit` nothing is negotiated: a build lacking what the selected
mode runs - its engine, `check --agreement`, and under `all` both engines
and `digest --form` - fails the check.

## OSC parser (`src/osc.rs`)

Minimal `.osc` / `.osc.gz` reader. `scan_osc_file` calls back once per
element entry with its (`<create>` / `<modify>` / `<delete>`) action, kind
and ID, in file order, and keeps nothing between entries; what is retained
is the caller's choice. Used by `verify_merge` for the delete-set carve-out
and the streamed classification of the element diff, and by `verify_diff`,
strictly, for the identities its input OSC touches.

Streaming hand-rolled tag scanner; gzip and plain XML are consumed in bounded
chunks, and the scanner retains only its current tag rather than the entire
decompressed file. It is tolerant of XML comments, processing
instructions, self-closing elements, and single-quoted attributes, and a `>`
inside a quoted attribute value does not end the tag (XML allows it raw). Element
bodies (tags / refs / members / coords / metadata) are deliberately skipped -
only IDs are needed. The document itself is checked: the first element must
be the `<osmChange>` root and the root must be closed, with nothing after it.
An empty file, a non-OSC document or a truncated diff is an error, not an
empty scan - `verify_merge` would otherwise report it as
"element-identical PASS". Entries are delivered before the document is known
to be whole, so a caller acts on them only once the scan returns `Ok`.

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
  without it, an existing key errors out. A `repack` promotion also records
  the new file as `same` as its source for `diff-snapshots` (Declared
  answers, above). Both flags are validated up-front
  via `download::preflight_snapshot_collision` (called from the top of
  `run_command_with_params`), so a forgotten `--replace-snapshot` errors
  before the cargo build kicks off, not after the run.

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
  fails with that refusal. `brokkr verify` differs (Verify subcommands).
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
  `--index-type sparse` or `auto`, `--compression`, and the measurement
  modes; pbfhogg refuses `--force` with `--inject-prepass` and with
  `--index-type external`, which need indexdata whatever `--force` says.
  brokkr does no validation of its own - pbfhogg owns the semantics. The
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
