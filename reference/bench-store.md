# The bench store

This page covers how brokkr stores script-bench runs, and the contract a
writer, a reader and a results server share. The code is
`src/bench/store/`. brokkr's side of it is `src/bench/evidence.rs`: where
the file lives, turning a sidecar capture into evidence, and credential
redaction. Typed-command history isn't here: it stays in each project's
`.brokkr/results.db` and `.brokkr/sidecar.db`. The run document, Derived,
not submitted, Publication, Marks and Reading define the store itself; On
the wire defines the results server protocol and brokkr's client; The
migration chain defines the schema.

## Where it lives

The store is one SQLite file per user, `$XDG_DATA_HOME/brokkr/bench.db`
(`user_dirs::bench_db`), shared by every project. Each run records its
project, and its checkout as data, since clones of one project share its
history. The file's housekeeping:

- `brokkr env` lists it, and names a configured results server.
- `brokkr check`'s free-space probe covers it.
- `brokkr clean` never removes it, and nothing prunes it.

The file must be on a local filesystem: opening it refuses a file where WAL
(write-ahead logging) doesn't take. SQLite on a network mount isn't
supported. A results server owns its file locally, and you reach it over the
network instead.

When the user-wide config names a server (`results_server`,
`docs/brokkr.toml.md`), brokkr uses it in place of this file for every
project. `brokkr bench` publishes there, `brokkr results` reads there, and
brokkr neither reads nor writes the local file. The two are one store behind
two backends, never merged: runs already in the local file stay there.

## The seam

`src/bench/store/` reaches only std, rusqlite, serde, serde_json and
xxhash-rust, never another brokkr module; the `bench-store-self-contained`
textlint rule enforces this. A second binary in this crate includes the
module by path, as the rustc guard includes the lock probe, and gets the
same document, validation, migration chain and transaction. A results
server is that module with a network layer in front, never a second schema.

The caller resolves the path, creates its directory and redacts
credentials, so a document is already redacted when it leaves the process
that made it. For the same reason the store's tests live outside the module
(`src/bench/store_tests.rs`, and `src/bench/wire_tests.rs` for the wire).
The exception is the frozen-step check, which needs nothing else.

That second binary is `brokkr-results-server` (`src/bin/results_server/`,
`reference/results-server.md`). It includes the module with a `#[path]`
attribute and serves its wire contract (On the wire, below). The wire is in
the module too (`wire.rs`), so the server and a client decode the same
types.

In the `brokkr` binary, the bench runner (`src/bench/runner/`) is the one
publisher of runs and `src/bench/marking.rs` the one writer of marks. Each
writes to the local store or, when the user-wide `results_server` names
one, through `src/bench/remote.rs`. The two share the retry admission and
uncertainty latch in `src/bench/publication.rs`.

## The run document

The unit of every write and read is one `RunDocument`: one attempted run,
whole. Attempted means an identity and a persisted outcome whether or not a
result came of it. A build that failed, a run interrupted before its first
iteration and a run whose iterations disagreed are all stored. The
subsections below describe each part of the document.

### Identity and conditions

A document's identity is:

- a client-generated uuid (32 lowercase hex)
- the project, bench and checkout
- the hostname
- the full commit sha, whether the tree was dirty, and the commit subject
- the start time
- brokkr's version and argv

Its conditions are the kernel, the CPU governor, available memory and the
captured environment, plus two host facts. Each is omitted when absent:

- **`previous_command`**: the newest row of your command history when the
  run started (`{command, ended_unix_s, elapsed_ms}`). The time is the
  row's own end, to the second, so it names the latest brokkr command to
  complete as far as that file knows.
- **`fstrim_timer_unix_ms`**: the mtime of systemd's `fstrim.timer` stamp.
  It records a timer firing, never a completed trim.

The `runs` columns are `previous_command` and `fstrim_timer_unix_ms`
(schema step 11).

### The build

A document always records what the build requested: target, profile,
features with host features folded in, and default features. It records
what resolution produced (the bin and cargo's resolved features) and the
artifact digest only as far as the build got. Three more fields follow.

**`lfs_pointers`**, for a `--commit` run, lists by path and oid every
tracked file of the worktree that was left as a Git LFS pointer when the
worktree was admitted. Absent means not recorded (a live-tree run, or a
document without the field); empty means scanned and none found.

**`lockfile`** is the build root's `Cargo.lock` as `{source, digest}`:

- `source` is one of `live`, `tracked`, `seeded`, `retained`, `fresh` or
  `unmanaged`, observed before the build.
- `digest` is its xxh3-128, read once the build succeeded. It's absent
  with no file, with a failed or unfinished build, and always for an
  `unmanaged` layout.

The `runs` column is `lockfile` (schema step 10), NULL when not recorded.

**`rustflags`**, for a bench with `rustflags` only, has two parts:

- `additions`: the resolved tokens.
- `placement`: how they reached cargo. That's `"none"` for an empty recipe,
  `{"env": {key, value}}` with the whole value cargo received, or
  `{"config": {args}}`. It's absent when the build stopped before brokkr
  chose one.

Publication refuses:

- a token that is empty or holds a NUL or 0x1f
- `none` beside tokens, or an injection beside none
- an injection that doesn't carry the tokens: an env key other than
  `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS`, or a value not ending with
  them; for config, anything but one `--config` entry at `build.rustflags`
  or `target."cfg(all())".rustflags` whose array is exactly them
- an artifact without a placement

The `runs` column is `rustflags` (schema step 13), NULL for every other
run.

### Measurement, params and config

The document records what you asked brokkr to measure: `timing`, `hotpath`
for `brokkr bench --hotpath`, or `alloc` for `brokkr bench --alloc`. A run
stored without the field reads as `timing`. The `runs` table keeps `alloc`
as `measurement = 'hotpath'` with its `alloc` column set. The measurement
column's CHECK can't be widened in place, and rebuilding the table would
cascade through every table that references it.

Params are stored by name. Each has `many`, its resolved `from` path, and
its values in bound order, each a key and, for a file, a digest. `from` is
`None` for a plain string. Otherwise it's the path into the run's resolved
view (`docs/brokkr.toml.bench.md`): `{param}` references substituted,
segments joined by dots, and a segment that isn't a bare TOML key quoted.
An unbound param is absent; a bound empty collection keeps its name and
path.

The configuration text the run was made from is stored once per distinct
text. The runner writes one TOML document holding the run's roots, the
script's exact source and origin, and the composed brokkr.toml, keys sorted
(`brokkr man bench`, What a run stores).

The document also has the iterations requested, the run's outcome
(`completed`, `failed` with a reason, or `interrupted`), and every iteration
that ran, in execution order.

### The schedule

A `brokkr bench --rounds` run has a `schedule`, omitted otherwise. It has
four parts:

- **`invocation`**: the id its invocation's runs share (32 lowercase hex).
- **`plan`**: the same in every run of the invocation. It holds the
  `axis` (`{"param": {"name"}}`, `"build"` or `"bench"`), each
  competitor's and workload's label, `rounds`, and `builds`. `builds` has
  one row per workload of each competitor's build index there, numbered
  from 0 in the order cells first need them: workload by workload,
  competitors in order.
- **`slot`**: `{workload, competitor, round, position}`. The position is
  the competitor's own index in an even round, reversed in an odd one.
- **`phase`**: `"measure"` for a cell, or `{"build": {"build"}}` for a
  build that failed or was interrupted. A build phase is attributed to the
  first slot it serves: round 0, the first cell of that build in that
  order.

Publication refuses:

- a plan of fewer than two competitors, or no workload
- 0 or more than 10 000 rounds, or more than 100 000 slots
- a `builds` row count other than the workloads', or a row length other
  than the competitors'
- builds not numbered as above
- a slot outside the plan, or not at the position its round gives
- a build-phase run that ran an iteration or completed

The `runs` columns are `schedule` and `invocation`, the latter indexed and
NULL exactly when `schedule` is (schema step 12). Keeping one invocation's
runs agreed on the plan, each slot held once, is the writer's job. The
reader (`brokkr results --invocation`) refuses runs that don't.

Protocols before 11 stored `builds` as one index per competitor. A run
stored that way still reads: its list is checked against the competitors
and the slot bound, taken as the row of every workload, and the result is
validated as a published schedule is. The store accepts that shape only
when it decodes its own `schedule` column: the wire and every new write use
the matrix.

### Iterations and the host trace

An iteration holds:

- its outcome: `completed` with a verdict, where nil and `""` differ;
  `failed` with a reason; or `interrupted`
- its block wall from the call of `bench()`, absent when `bench()` was
  never called
- its `record` as extracted at the end of the sample, a JSON object.
  Absent means extraction never happened, which isn't the same as an
  extracted empty table.
- its children in launch order
- its **host trace** (`host`)

The host trace is absent, and omitted, for an iteration never admitted and
for every iteration stored without the field. Every time in it is
`CLOCK_BOOTTIME` in microseconds, read straight after the observation it
stamps, so one boot's readings order and subtract across iterations, runs
and projects. The trace holds:

- the boot id
- the block device sampled for the iteration container, absent with no
  single block device, as on btrfs
- `previous`: the cell cursor as the iteration found it (The cell cursor,
  below), and whether the iteration's own in-progress cursor was written
- `block_start_us`: when `bench()` was called, read on the boot clock as it
  was; absent when it never was, or when that read failed. A child's
  `origin_offset_us` counts from the same moment on the monotonic clock, so
  the sum places a child on the trace only while the host stays awake; a
  child's boot launch reading (Children, Boot interval) places it exactly.
- a boundary or periodic reading whose boot clock read failed is absent,
  never stamped zero
- the `start` reading before `bench()`, and the `end` reading after it
  returned
- the `released` reading after the iteration directory's release, with
  whether that release completed. When it didn't, brokkr left the
  directory to a later scan, and cleanup I/O may follow.
- the periodic readings between, every 100 ms from a thread of their own,
  independent of any child
- `capped` when the run's 100 000 periodic readings ran out. brokkr keeps
  the boundary readings and cursor past the cap, a few hundred bytes per
  iteration.

The device is `{name, major, minor, layer, diskseq}`. `layer` is `disk` for
a whole disk, or for a partition promoted to its disk so its counters
include sibling partitions. It's `mapped` for a dm, md or loop node, whose
counters are its own. A reading keeps a device's counters only while the
device's sysfs name still holds the numbers and `diskseq` resolved.

A reading is `{at_us, dirty_kb, writeback_kb, device}`. The memory parts are
`/proc/meminfo`'s `Dirty` and `Writeback`. The device part is its cumulative
`stat` counters `{sectors_written, writes_completed, write_ticks_ms,
in_flight, sectors_discarded}`, discards absent on a kernel without them.
Each part is absent when its read failed, never zero.

Publication refuses:

- a negative reading, or readings out of order
- counters with no device, the trace's or the predecessor's
- a negative block start
- a predecessor of the same boot whose end is stamped after this start

The writer checks its own trace against those rules first. It drops a
predecessor that would be refused, and doesn't store a trace still refused,
so no cursor file costs a run its publication.

The `iterations` column `host` holds the trace without its periodic
readings. Those are `host_readings` rows by ordinal (schema step 11), read
only with children: a summary's traces keep their device, cursor and
boundary readings, and have no periodic ones.

### The cell cursor

The **cell cursor** is a per-user file beside the global lock
(`bench-cell.json` in brokkr's state directory), and only a holder of the
lock writes it. An iteration reads it and replaces it with `in_progress`
naming itself (`{run, project, bench, iteration, boot_id}`). After its
release, the iteration replaces it with `ended`, carrying its device and
`released` reading. When that reading's boot clock read failed, there is
no stamp to end on, so the `in_progress` entry stays and brokkr warns.

So `previous` names the last cell to start or end on the host, whatever its
project, destination or publication. `in_progress` means that cell never
recorded its end, so the gap and what happened in it are unknown.

A failed in-progress write is recorded as `cursor_written` false, and
brokkr removes the older entry, so a later cell finds none rather than
taking it as its predecessor. Only when the removal fails too can a later
cell read the older entry across this one unseen. Replacement is a rename,
so the guarantee covers process death, not power loss.

A reader shows the gap: elapsed time and device deltas from the
predecessor's `released` reading to this `start`. With no `start` reading,
it still names the predecessor and calls the gap unknown. It's derived,
never stored, under three rules:

- elapsed time only within one boot
- device deltas only when both sides name one device with one `diskseq`
- no deltas across a counter regression

### Children

A child holds:

- its program, argv and environment additions, and pid
- its origin, offset from the start of its iteration's block on the
  monotonic clock, and its wall
- its ending: exited, signal, stop marker, deadline (a whole `Duration` in
  seconds and nanoseconds), interrupted, or wait failed. The ending is
  evidence only, since the script decides what an exit means.
- its evidence: the `/proc` samples, every FIFO record in one arrival order
  (markers, counters, sealing emissions and terminal records, with their
  call stream), the capture loss by cause, the clock check, and the
  call-id census

Stamps stay on the child's own clock and are never rebased; an off-contract
stamp is kept as it came. Library call boundaries aren't stored: only a
library call document supplies them, and a script bench parses none.

The fields below are each omitted when absent, so a child without one
encodes as a child stored before the field existed.

### Capture

A child of a call marked `hotpath` also holds its **capture**: the stage
name, its occurrence, and the outcome. The occurrence counts the stage's
marked calls so far in the iteration, from 1, whatever became of each, so
within an iteration a stage's occurrences run 1..n in launch order. The
outcome is one of:

- `captured` with the report: a JSON object as the child wrote it and the
  reader's parser takes it, nested at most 64 deep, as a record
- `missing`, `invalid` or `unreadable`, each with its reason
- `over_budget`

An unmarked child has none. A `completed` run measured `hotpath` or `alloc`
holds a capture in every iteration, and every capture it holds is
`captured`. Any other run may hold any outcome.

### Executable

A child of a `program` call, which ran an external program in place of the
built snapshot, also holds its **executable**: the absolute path brokkr
resolved and launched. With it is either that file's xxh3-128 digest as
read at launch, or why brokkr couldn't read it. It's evidence of what was
there, not a pin: brokkr doesn't see a file replaced between the digest and
the exec.

Absent means no separate observation: the snapshot child, whose identity is
the build's artifact, and every child stored before the field existed. It
isn't part of the pairing key. `brokkr results --compare` names the
executables whose observed identities differ.

### Stderr

A child whose stderr brokkr spooled also holds its **stderr**:

- whether its call's borrow was released before the snapshot. Without the
  release, something the child started may still have been writing, so
  what follows is what the spool held then.
- the spool's size as observed, absent when brokkr couldn't observe it
- its tail

The tail is one of:

- `kept`, with the count of raw bytes kept (the last 64 KiB at most) and
  their text, each invalid UTF-8 sequence replaced. `cut` is set when
  earlier bytes weren't kept, so the text may open mid-line.
- `over_budget`, when the run's 16 MiB of raw tail bytes had run out. An
  empty stderr is always kept.
- `unreadable`, with its reason

It's evidence of how a child's stderr ended, never its whole log. Absent
means not observed: every child stored before the field existed.
Publication refuses:

- a kept tail without a size
- a raw count past 64 KiB or past the size
- a `cut` that isn't exactly "fewer raw bytes than the size"
- a text shorter than its raw count, or over three times it (the most
  replacement expands to)
- a run whose kept raw bytes pass 16 MiB

### Measured

A child of a call marked `measured` also holds the **measured** flag: its
wall is its iteration's result (Derived, not submitted). An iteration holds
at most one. A `completed` run's iterations either all hold one or none
does; a `failed` or `interrupted` run may stop before a later iteration
reached its marked call. The flag is omitted when unset, so every unmarked
child encodes as one stored before the flag. The `children` table keeps it
as a `measured` column (schema step 7), defaulting to unset.

### Cache

A child of a call that prepared inputs `cold` or `warm`
(`docs/commands/bench.md`, Page cache) also holds its **cache** account:

- the `procedure` it followed, `page-cache-v1`, so a later procedure's
  counts are never read as this one's
- the scan's start and end, from the block's start
- per prepared inode, in the order the call listed them: every path listed
  for it, its mode, its size, and the bytes the scan found resident

The counts accumulate over the scan interval; they aren't one snapshot.
Absent means brokkr recorded no preparation, never a residency: a call that
asked for none, and every child stored before the field existed.

Publication refuses:

- an empty procedure or file list
- a scan that doesn't end by the child's launch
- a path that isn't absolute, or a size past SQLite's range
- a count the mode rules out: `cold` with any byte resident, `warm` with
  fewer than all

The last rule holds because a preparation that didn't establish its mode
fails its iteration and stores no child. Schema step 8 keeps the account as
`child_cache`, `child_cache_file` and `child_cache_path`. It isn't part of
the pairing key; `brokkr results --compare` names it when the sides'
sequences of preparations differ.

### Boot interval

A child also holds its **boot** interval, on `CLOCK_BOOTTIME` in
microseconds, the clock its stamps count on (`docs/commands/measure.md`, The
clock):

- `launch_us`: the reading brokkr took just before the spawn, the child
  clock's zero, absolute like the host trace's readings
- `interval_us`: the span from it to the end observation the wall also
  stamps, absent when the boot clock could not be read there

The interval differs from the wall by any suspend, and by the skew of
reading two clocks in turn. A reader places the child on the host trace at
`launch_us`, and closes its timeline at `interval_us`, or at the wall when
that is absent. A child stored before the field existed has neither, and a
reader falls back to the wall and to the block start plus the origin offset,
which a suspend misplaces.

Publication refuses a negative reading or span. Schema step 14 keeps them as
the `children` columns `boot_launch_us` and `boot_interval_us`; the reader
refuses a span without a launch. Neither is part of the pairing key.

### Format and strictness

Every struct refuses unknown fields, so the store refuses a document a newer
writer extended rather than storing it short of its new fields. That
refusal, not `format`, keeps an older store from taking a newer document.

The document also has `format`, which moves only when what a document
already says would read differently. An optional field added and omitted
when absent (a child's `executable`) leaves `format` alone, since every
document without it means what it always did. The outcome and ending enums
are externally tagged for the same reason: an internally tagged unit
variant would drop any field beside its tag.

Publication also refuses what wouldn't read back as it came:

- a record or a captured report nesting deeper than 64 levels; the
  reader's JSON parser stops at 128
- a capture count past SQLite's signed 64-bit range

## Derived, not submitted

The store computes what follows from the document, so every writer agrees
and a replayed document compares equal to the stored one. It derives:

- **The result.** A run has one exactly when its outcome is `completed`.
  The result is the first iteration with the smallest result wall: its
  measured child's wall when it marked one, else its block wall.
  Publication refuses a `completed` run unless it holds every requested
  iteration, each completed with a wall and an extracted record, all with
  one verdict, and a built artifact. A `failed` run may hold only completed
  iterations (a verdict disagreement, or a failure after the last one). A
  `failed` or `interrupted` run may hold none. Iterations of a run without
  a result are evidence only, and never pair or compare.
- **Positions.** A sample's index is its position. A record's arrival index
  is its position across every kind. A marker's index is its position among
  markers.
- **The pairing key**, described below.
- **The config digest**: xxh3-128 of the text, as 32 hex digits. A digest
  already naming a different text is refused, not shared.

The pairing key is the project, the bench, and each bound param by name
with its `many` flag and its values' keys and digests in bound order. It's
tagged by what the run measured:

- A run measured `timing` none of whose children, in any iteration,
  captured a report is keyed `bench-pair-v1`. That's the key every run had
  before captures existed, so that history keeps pairing.
- A run measured `hotpath` is keyed `bench-pair-v2` with the class
  `hotpath`, and one measured `alloc` with the class `alloc`.
- A `timing` run that captured anyway, its build instrumented by other
  means, is keyed `bench-pair-v2` with `incidental`. No two classes ever
  pair.
- A run any of whose iterations marked a measured child is keyed
  `bench-pair-v3` with its class and `measured-child`, so a run ranked on a
  child's wall never pairs with one ranked on the block's.

Collection order, a digest change, whether a param is bound, and the class
all split the key. The commit, the config text, the build and the record
don't. The key is a projection of the stored params and children: a later
algorithm is a new projection computed from them. Two rows compare only
under one algorithm, never by matching one tag's string against another's.

## Publication

`Store::publish` validates the document, then under one `BEGIN IMMEDIATE`
looks up its uuid, and either inserts every row or none. The uuid is the
request id:

- A uuid already stored is answered `AlreadyStored` when the stored
  document equals the submitted one.
- It's refused as a conflict when the documents differ.
- A stored run that can't be read back is a store failure, never a
  conflict.

The answer to a replay is reconstructible from the run itself, so no
separate request record exists for this write kind. A write kind that
deletes runs would need one, since a deleted run can no longer answer its
publication's replay.

Errors say what a writer may conclude:

- **`Refused`** (the request, the store's version or the file),
  **`Conflict`** and **`Failed`** (anything before the commit): known
  unapplied.
- **`CommitUncertain`** (the commit itself errored): may have been
  applied. Republishing the same document settles it.

## Marks

What you say about stored runs afterwards lives beside them, never in them
and never in `brokkr.toml`. A stored run is never edited or deleted, and a
bench retired here stays hidden after its table is gone from the config.
The code is `marks.rs`.

**The log.** `Store::mark` appends one `MarkRequest` to the `marks` table,
the exact request as submitted: a client uuid, the project, when and on
which host it was made, and the mark. The marks are:

- `retire` a bench name, and `unretire` it
- `invalidate` a run, by its full uuid, and `revalidate` it
- `rename` a bench name

`retire` and `invalidate` require a non-blank reason; the undos take none.
The mark uuid is the request id, as a run's is its publication's, under one
`BEGIN IMMEDIATE`. A uuid already stored is answered `AlreadyApplied` when
the stored request is equal, and refused as a conflict when not, an invalid
request included. That lookup comes before any check but the uuid's own
shape, so a request a later mark undid still answers its replay rather than
being refused or applied again. Errors read as in Publication.
`Store::marks` returns a project's log in apply order.

**The fold.** What the marks mean is one fold over a project's log in row
order (`MarkView`): the renames in effect, the retired end names, the
invalidated runs, and each retirement and invalidation with the request it
came from. Admission checks a proposed mark against that fold and the runs
stored at that moment, and refuses as `Refused`. The fold applies every
accepted mark unconditionally and never consults runs, so a mark does on
every later read what it did when admitted. The fold's rules:

- **Renames** form chains resolved to their end (`MarkView::name`). A name
  is the source of at most one rename, and only a name the store knows (it
  has runs, or a mark in effect names it) can be renamed. Renaming onto a
  known name is refused, so histories never merge silently. The exception
  is renaming onto an ancestor in the source's own chain: that's the undo,
  and it cancels every rename on the path, so the view never holds a
  cycle. The no-merge check sees the runs stored when the rename commits. A
  run of the new name published afterwards, one already in flight
  included, joins the renamed history, since a publication has no identity
  but its name.
- **Retirement** belongs to an end name, and a rename never changes any
  run's visibility. A forward rename moves the retirement to the new end.
  An undo gives every name on the cancelled path a copy of it, each then
  unretired on its own. Retiring or unretiring a renamed name acts on its
  end. A run published later under a retired name is retired too.
- A run is **hidden** when it's invalidated or its resolved name is
  retired.

Admission refuses:

- a retirement of an unknown or retired name
- an unretire of a name not retired
- an invalidation of a run not in the project, or already invalidated
- a revalidation of a run that isn't invalidated

**Pairing across a rename.** The stored pairing key keeps the bench's
literal name and its contract (Derived, not submitted).
`MarkView::comparison_key` is a separate, view-dependent projection: the
stored key with its bench element replaced by the resolved name, its tag
and class kept, so two algorithms' keys still never meet. A key of a shape
it doesn't know comes back unchanged. It's computed from the stored key,
never from a summary's document, whose children aren't there to give its
class. `brokkr results --compare` groups a side's runs by resolved name and
comparison key under its selection's one view. Every view shows the
resolved name and discloses a differing stored one. Anything read out of a
run's stored config (its annotations) is read under the stored name.

**Revisions.** Runs are inserted whole and never deleted, and marks are only
appended, so a `Revision` fixes one history. A revision is the highest mark
row and the highest run row a read sees: two bounds, since neither orders
the other's rows. A selection is made at one revision, in one read snapshot
with the marks it folds, and answers it. A second selection pinned to it
sees the same runs and marks whatever was written in between. A revision
past the store's current one is refused, since it would admit rows written
later.

## Reading

`Store::open_read_only` opens an existing store and never creates its file:
a missing file is `None`, which a reader treats as no history. A store at
the current version opens read-only and `query_only`. One on an older
version is migrated first, then reopened read-only. The migration uses a
writable connection opened without `SQLITE_OPEN_CREATE`, so a file removed
in between is an error, not a fresh store. A newer or negative stamp, and a
file with tables but no stamp, are refused, as by `Store::open`. The
promise is about the database file: SQLite may make the `-wal` and `-shm`
files a WAL read needs.

The listing queries live here, not in the reader, so a results server
answers the same queries from the same SQL. `RunFilter` always names a
project, and selects by:

- uuid prefix
- commit prefix, of the full sha
- bench substring, matched against the run's resolved name (Marks)
- captured env
- a bound param's value, its keys joined by `,` in bound order
- literal invocation terms: brokkr's argv, every child's argv and
  environment additions, and the captured env, each element matched as
  stored rather than as JSON text
- whether the run has a result, or a clean tree
- the `--rounds` invocation whose id starts with a prefix
  (`invocation_prefix`), which only a scheduled run answers

Hidden runs are left out unless `hidden` is set. A uuid lookup sets it, so
a hidden run is still found by its uuid. `Store::select` answers a
`Selection`: the uuids, the revision it was made at, and the `MarkView` in
effect there, which a reader resolves every run it shows against.
`has_runs` and `elsewhere` count every run, hidden or not.

Every listing is in one total order, newest first by `started_unix_ms` then
uuid, never by insertion, since a publication may arrive late or be
replayed from another client. `Store::run` returns a run with what the
store derived (config digest, pairing key, result iteration), its children
read only on request.

Two runs pair when their stored pairing keys are equal. The algorithm tag is
inside the key, so rows of two algorithms never match; a reader that needs
the tag reads it from the key.

## On the wire

`wire.rs` is the contract a results server and its clients share, with no
HTTP in it. There's one route:

- **`POST /bench`** takes one request and answers it.
- **`GET /bench`** answers the server's identity without touching the
  store: `server`, `version`, `protocol`, and an echoed `probe`. From
  `brokkr-results-server` it also has `build`, the executable digest, and
  `stamp`, its build stamp, when it has one.

A client ignores fields it doesn't know, so adding `stamp` needed no
protocol step. Every request sends the `brokkr-bench-protocol` header
naming `PROTOCOL_VERSION`, and every answer the server writes sends it back.

**Requests and answers** are externally tagged, so a large document decodes
in place rather than being buffered to find its tag. The kinds:

- `publish`: a whole `RunDocument`. Its uuid is the request id, as in
  Publication above.
- `select`: a `RunFilter` and an optional revision to pin it to, answered
  `selected` with a `Selection`.
- `run`: a uuid and whether to read children, answered with a `StoredRun`
  or null.
- `has_runs` and `elsewhere`: each answered by the variant of its name.
- `mark`: a `MarkRequest`, answered `marked`. Its uuid is the request id,
  as in Marks above.
- `marks`: a project, answered with its log.

A run read without children (`children_loaded` false) is a summary. Its
iterations' children are empty because they weren't read, so it's never a
document to republish, and a client checks the flag is what it asked for.
`wire::execute` runs a decoded request on a `Store`. `wire::serve` decodes a
body, executes it and encodes the reply, which is all a server does with a
body once it holds the store.

**Strict, one protocol at a time.** Requests and answers refuse unknown
fields and require every non-optional one, in both directions; there are no
versioned adapters. Any change to how a request, an answer or a type inside
them (the run document included) encodes or decodes is a new
`PROTOCOL_VERSION`. A request naming another version is refused as
`bad_request` naming both, so you upgrade client and server in step.

`SHIPPED_PROTOCOLS` holds one fingerprint per version: the xxh3-128 of the
canonical encoding of a frozen set of fixtures covering every request and
answer kind and every failure kind. A shipped entry never changes, and a
test fails when the encoding no longer matches the last one. The
fingerprint sees only those fixtures' encoding, so a second test holds the
accepted input language. That test checks that brokkr refuses:

- unknown fields, at the top and nested
- a missing required field
- an out-of-range integer
- an unknown request kind

The route, the header and the request cap are pinned outside any version,
since a client of any version must find the server.

**The failure envelope** is the one tolerant shape, and its meaning is
stable across protocol versions, since a client meeting a server of another
version must still read the refusal. The shape is
`{"error": {"kind", "message", "applied", "retry"}}`:

- `retry` is required.
- `applied` is present, and `false`, only when non-application of this
  attempt is proved.
- Extra fields, and kinds a client doesn't know, are tolerated.
- A duplicated field makes the envelope unreadable.

The kinds:

| Kind | Applied | Retry | Meaning |
| --- | --- | --- | --- |
| `refused` | false | no | `StoreError::Refused`: an invalid document. |
| `conflict` | false | no | `StoreError::Conflict`: uuid stored, other content. |
| `failed` | false | yes | `StoreError::Failed`: failed before any commit. |
| `commit_uncertain` | absent | yes | `StoreError::CommitUncertain`. |
| `bad_request` | false | no | Malformed, unknown kind, other protocol, no length. |
| `too_large` | false | no | Over `MAX_REQUEST_BYTES` (256 MiB). |
| `busy` | false | yes | At capacity, or the worker gone; never run. |
| `read_timeout` | false | yes | The body didn't arrive in time. |
| `wait_expired` | false or absent | yes | The server stopped waiting. |
| `internal` | absent | yes | Failed after the request started. |

Three rows need more than the table holds:

- `commit_uncertain` means the commit itself errored.
- `wait_expired` has `applied: false` only when the request was withdrawn
  before it started.
- `internal` covers a panic and an unencodable answer.

A client reads the envelope through `Failure::classify`:

- `applied: false` proves this attempt unapplied.
- `applied: true` is no value the protocol uses, and reads as uncertain.
- A known kind whose `applied` or `retry` contradicts the table proves
  nothing, and reads as uncertain.
- An unknown kind is read by its `applied` and `retry` alone.
- A `StoreError` is rebuilt only for a store kind whose `applied` and
  `retry` are exactly that kind's (`conflict` with the uuid the client
  published). A contradiction never reaches a caller as a store error whose
  class claims more than the envelope proves.
- A body without a well-formed envelope (a proxy's page, hyper's own
  refusal of malformed HTTP, a truncated answer) is classified by its
  status alone, whatever its headers.

**What a client owes the contract.** `applied: false` speaks for that
attempt only. A publication can become uncertain through no answer,
`commit_uncertain`, an uncertain `wait_expired`, `internal`, or a 5xx
without an envelope. Once any attempt is uncertain, the publication stays
uncertain across later refusals and proved-unapplied attempts, since the
uncertain one may still commit. Only a successful answer (`stored` or
`already_stored`) settles it, and republishing the same document is how to
get one. A 200 to a publication acknowledges it even when its body can't
be read.

The HTTP status is advisory (400, 404, 405, 408, 409, 411, 413, 500, 503,
504); the envelope decides. A client checks a run answer's
`children_loaded` against what it asked, and sends the protocol header on
every request.

**brokkr's client** (`src/bench/remote.rs`) keeps those duties, with these
rules of its own:

- **By status alone**, for an answer without a well-formed envelope: a 4xx
  is proved unapplied and final, and 408 and 429 are unapplied and
  retryable. A 4xx without an envelope was refused before a brokkr server
  ran the request, by a proxy or by the HTTP layer on malformed HTTP; this
  assumes an intermediary uses its statuses faithfully. Anything else, a
  proxy's bare 502 or 503 included, is uncertain and retryable.
- **A transport error** is proved unapplied only when the connection never
  existed: the name didn't resolve, connecting timed out, or the
  connection was refused. Each attempt makes one fresh connection (no
  redirects, no idle pool, no retry inside the HTTP client), and on Linux a
  TCP socket reports a refusal only before it connected. Every other
  transport error is uncertain.
- **An attempt is never cut short of the server's deadline.** An attempt
  may take `wire::CONNECTION_DEADLINE` (the server's connection deadline,
  300s) plus 30s for a proxy, so a server that isn't stuck always answers
  or closes first.
- **Retries** (`src/bench/publication.rs`) start only inside a window from
  the first attempt and under a cap: 120s and 8 attempts for a
  publication, 20s and 6 for a read or the probe, with a backoff from
  250 ms. A publication whose uncertainty is latched gets one attempt past
  either limit, so an attempt that timed out late still has a
  republication to settle it.
- **Retries end** on a final failure. A pending stop ends them after the
  first attempt, during a backoff too, but never cuts the attempt in
  flight. brokkr takes no pause when no attempt would follow it, and checks
  admission again after the pause.
- **The bounds** cover network waiting and backoff, not the command. A
  publication's worst case is its window plus two whole attempts plus the
  pause before the last (at most 8s). A read's is its window plus one
  whole attempt plus a pause (at most 4s).
- **Before a run is measured**, `brokkr bench` probes the server
  (`GET /bench`, its protocol checked), as it opens the local store first.
  A server that can't be reached, or speaks another protocol, fails the
  command before anything is built. The probe proves reachability, not that
  the publication will be stored.
- **Reads** are retried without a latch, since they're idempotent, and a
  200 whose body doesn't decode as the expected answer is final. A listing
  costs one `run` request per run, since the protocol has no batch read.

The same latch serves the local store. brokkr classifies a `StoreError` by
the kind it would travel as (`Kind::of`), so a local `Failed` is retried,
and a final error after an uncertain attempt ends the publication
uncertain.

**Marks** (`src/bench/marking.rs`) are written under the same latch and the
publication policy, as one request minted once per command. Every attempt
sends the same uuid, time, host and encoded bytes, and a conflict is read
under the mark's uuid. Only a success settles it: locally `Applied` or
`AlreadyApplied`, remotely any HTTP 200, whatever its body. Running a
command again makes a new request with a new uuid, never a replay. So
brokkr answers an uncertain end by reading the current state, which proves
nothing about whether the uncertain attempt may still commit. A local mark
needs an existing store (`Store::open_existing`, which never creates the
file). With none, nothing has been published to mark, and the command
fails.

## The migration chain

`schema.rs`'s `MIGRATIONS` owns the schema. Step N takes `user_version` N to
N + 1, and every pending step applies under one `BEGIN IMMEDIATE`, the stamp
re-read inside it. A shipped step never changes, since every store and a
server replay the same chain. A schema change is a new step, and the
frozen-step test pins each shipped step's text by digest.

Sample columns are spelled out in the step that adds them. A test holds
them to the store's sample fields and those to the sidecar's, so a new
`/proc` field needs a new step. Opening refuses a store stamped newer than
the code, and a file with tables but no stamp, which isn't a bench store.
Tables are `STRICT`, and the checks that tie an outcome to its result,
failure and verdict columns are in the schema as well as in validation.
