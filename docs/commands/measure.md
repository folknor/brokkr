# Measurement and the sidecar

This page covers the sidecar profiler brokkr attaches to a measured child,
`brokkr sidecar` queries, the marker FIFO, and how the typed measurement
history in `results.db` and `sidecar.db` still reads. "Stored history"
explains what is left of the typed commands. "Sidecar" through "Hotpath
JSON" are the reference for captured data and `brokkr sidecar` queries, and
"Refusals" lists what they refuse.

## Stored history

Every project measures through script benches (`brokkr bench NAME`, see
`brokkr man bench`), which keep their runs in the bench store. No brokkr
command takes the typed `--bench`, `--hotpath`, `--alloc` or `--stop` mode
flags any more, and nothing writes `.brokkr/results.db` or
`.brokkr/sidecar.db`. What earlier typed runs stored there keeps reading:
`brokkr results` lists and compares those rows, `brokkr sidecar` queries
their trajectories, and `brokkr invalidate` deletes them.

A typed row is one best-of-N measurement. Its `mode` column is `bench`,
`hotpath` or `alloc`, and its `elapsed_ms` is the wall rounded to the
nearest millisecond, with the exact `elapsed_us` beside it where the run
knew it. A row's trajectory in sidecar.db shares its id. A run that filed no
row (a dirty tree, a failed insert, an interrupt) kept its trajectory under
a random id, and the `dirty` alias resolves to the most recent such one.

### Iteration walls

A `--bench N` row kept every iteration's wall in the `run_iterations` table
(`run_id`, `run_idx`, `elapsed_ms`, `elapsed_us`), in execution order.
`brokkr results UUID` renders them like this:

```text
elapsed  592200.412 ms (best of 3: 592200.412 / 611400.090 / 634100.733 ms)
```

The walls are never sorted, because the order is the signal: iteration 1
fastest and 3 slowest is drift under the run, iteration 1 slowest is a cold
page cache on the first pass. `elapsed_us` is NULL unless every iteration
had a microsecond reading, and rows without microseconds render the integer
milliseconds. Rows recorded before schema v16 have no iteration data, and
rows before schema v19 no per-iteration microseconds.

### prev fields

A typed row also recorded its predecessor as `prev.*` kv pairs:
`prev.command`, `prev.uuid`, `prev.input_file` and `prev.gap_seconds`, from
the most recent row in the same project's results DB. `prev.gap_seconds`
runs from the previous run's timestamp to the moment this run started, so
it's a proxy for page-cache state: together with `prev.input_file` it tells
"this baseline was warm" from "this baseline was fast".

## Sidecar

The sidecar profiler samples a measured child's `/proc/PID/stat`,
`/proc/PID/io` and `/proc/PID/status` every 100 ms and reads phase markers
from a FIFO. It buffers its data in memory during the run and hands it to
the script-bench runner after the child exits, which stores it with the run
in the bench store.

The child gets a `BROKKR_MARKER_FIFO` env var pointing to a named pipe,
`.brokkr/.sidecar-TOKEN.fifo`, with a fresh 128-bit random token per
measured child. `brokkr man clean` says what `brokkr clean` removes.

Only a child brokkr attaches a FIFO to gets the variable. brokkr drops
`BROKKR_MARKER_FIFO` and `BROKKR_HARNESS_ARTEFACT_DIR` from its own
environment when a command starts. The exception is the test harness shim,
which forwards its environment unchanged to the program cargo meant to run.
So a brokkr started under another brokkr's sidecar passes that FIFO to none
of its children, measured or not. Configured environment can still set the
name explicitly.

Sidecar data is kept even when the child fails: OOM, signal, deadline, or
non-zero exit (see "Crash preservation").

## Sidecar queries

`brokkr sidecar UUID` bare prints the per-phase JSONL summary; pass `--human`
for a table. The view selectors are mutually exclusive:

- **`--samples`**: raw JSONL `/proc` samples
- **`--markers`**: raw JSONL marker events
- **`--durations`**: START/END pair timings. In `--human`, repeated span
  names collapse to one aggregate row each; JSONL stays one object per span.
- **`--counters`**: application counters; `--grep SUBSTR` filters by name
- **`--stalls`**: `*_wait_ns` rollup (see "Counter views")
- **`--stat FIELD`**: min, max, avg, p50, p95
- **`--compare A B`**: two runs' phases side by side, a repeated name paired
  by ordinal (see "Rendering")

`--call CALL_ID` isn't a view: it restricts any view but `--compare` to one
call stream.

Filter flags `--phase`, `--range` and `--where` compose with `--samples` and
`--stat`. `--fields`, `--every`, `--head` and `--tail` compose only with
`--samples`.

A UUID is required except for `--compare`. The `dirty` pseudo-UUID resolves
to the most recent typed run that filed no row (see "Stored history"), and
each earlier one by its own id. A typed `--bench N` session holds all N runs
in sidecar.db but marks `best_run_idx`, the run whose wall clock matched the
reported `elapsed_ms`. The default view and `--stat` and `--phase` read that
best run unless another `run_idx` is selected.

The UUID argument is a prefix, matched literally (`_` and `%` aren't
wildcards), and it must name one session. A prefix shared by several sessions
is an error listing the candidates rather than a merged timeline, and an
empty prefix is refused.

The prefix resolves across `results.db`, `sidecar.db` and the current
project's script-bench runs at once (see "Script-bench children"). So a
prefix naming a typed run and a bench run is ambiguous too, the bench
candidates marked `(script bench)`. A typed run counts whether or not it has
sidecar data. A prefix naming only another project's bench run says which
project it belongs to. Because the query reads the bench store, a typed query
fails when that store can't be read, or the configured results server can't
be reached, as `brokkr results` does.

## Script-bench children

A script-bench run (`brokkr man bench`) stores what the sidecar captured for
every child of every iteration. That covers samples, markers, counters, sink
ends, capture losses by cause and the clock check (`brokkr man results`,
Script benches). `brokkr sidecar UUID` reads one child of each selected
iteration through every view in "Sidecar queries". It shows the child's
records under its own clock as they were written, nothing rebased.

### Selecting a child

Two flags pick the child:

- **`--run N`** selects the iteration (`all` for every one). By default it's
  the result's iteration, or for a run without a result the last one stored.
- **`--child N`** selects the child within each selected iteration, in
  launch order. By default it's the iteration's only child.

brokkr judges the whole selection before anything prints. An iteration that
launched several children without `--child`, none, or fewer than `--child`
names refuses the query, naming every such iteration. One selected iteration
that launched no child, with no `--child`, prints its outcome and `launched
no child` instead. `--child` on a typed run is refused.

In JSONL, `run` is the iteration, in sample records too; a typed sample has
none. The provenance header on stderr names the bench, project, start, commit
and outcome. It then names the iteration, the child's argv, PID, start offset
in the block, wall and any abnormal ending. Each selected child's capture
loss is warned with its exact causes, and its clock check as for a typed run,
labelled `--run N --child N`.

### Child timeline

A child's interval is known, from its spawn to its stored end. brokkr keeps
that end on the boot clock its stamps count on, so it matches the wall
unless the run spanned a suspend. A child stored before it was kept, or
whose end brokkr couldn't read on that clock, ends at its wall. So its process timeline closes at that end, not at its last
sample:

- The last phase ends there, or at a marker stamped past it, which moves
  only the segment end.
- A markerless child is one `(all)` segment from 0 to the end.
- The default summary and `--compare` need no sample.

A phase no sample fell in is still a phase. The `--human` table shows it with
`-` readings, JSONL with `samples: 0`, and a `--compare` side gives its
readings as `null`, never zero. Phase records and `--compare` sides add
`end_us` and `duration_us`. The summary record's `wall_ms` and `wall_us` are
the child's wall, with `samples` and `peak_threads` (`null` without a
sample). `--stalls` reads against the child's wall.

A child's call streams have no stored boundaries. So the default summary adds
a `call_sink` record for a call whose sink ends arrived, which a typed run
without boundaries doesn't print. The record has `run`, `call`, the terminal
record that arrived or `null`, and the sealing count.

`--call` knows a call by its records or the child's census of call ids. One
only the census names is said to have records the capture didn't keep. An
unknown one under an overflowed census is said to be possibly unrecorded.

### Child compare

`--compare` takes `UUID`, `UUID/ITER` or `UUID/ITER/CHILD` per side, each
side on its own. A bench side is its result's iteration and that iteration's
only child, unless the operand names others. A run without a result never
compares, as `brokkr results --compare` holds. A typed side takes no
qualifier and is its best run. Typed and bench sides mix, both being one
process stream. `compare_summary` gains `a_iteration` and `a_child` (and the
`b_` pair) for a bench side, and the table heads it `uuid/ITER/CHILD`.

### Host windows

A child of an iteration stored with a host trace (`brokkr man bench`, What a
run stores) gets each phase's host window. That's the iteration's host
readings stamped inside the phase, on the child's clock. A phase record and
a `--compare` side add `host`, with these fields:

- `readings`
- `capped`, when the run's readings ran out in that iteration so later ones
  are missing
- `peak_dirty_kb`, `peak_writeback_kb`, `peak_in_flight`
- `device_written_kb`, `device_discarded_kb`, `write_wait_ms`

The `--human` tables add a `host:` line under the phase.

The device figures are deltas between the window's first and last readings.
They're `null` with fewer than two readings, across a counter reset, or, for
the wait, with no write completed. The wait is block-layer write wait per
completed request, queueing included, and a request completing inside the
phase may have waited before it.

These are host-wide readings: other work on the host and sibling partitions
of the disk count. Both the trace and the child's stamps are on the boot
clock, and the child's zero is the boot reading brokkr took just before
spawning it, so a suspend during the iteration doesn't move the windows.
Only a suspend inside the spawn itself, between that reading and the
process's start, still shifts them by its length. A child stored before
that reading was kept is placed at its block start plus its launch offset,
which a suspend between the two misplaces too. No sample
field names the host readings, so `--fields`, `--where` and `--stat` don't
reach them.

Every human table prints whole milliseconds, so a sub-millisecond phase of a
short child reads `0ms` there; the JSONL has microseconds.

## Sample fields

Each 100 ms sample is assembled from three files, and **all three reads must
succeed or the whole sample is dropped**: a partial read at process exit
would corrupt phase deltas. The field names below are the tokens `--fields`,
`--where` and `--stat` accept. `t` is always present in `--samples` JSON.

| Field | Source | Meaning |
|---|---|---|
| `i` | none | sample index |
| `t` | none | seconds since spawn (see below) |
| `rss` | status `VmRSS` | resident set (kB) |
| `anon` | status `RssAnon` | anonymous RSS (kB) |
| `file` | status `RssFile` | file-backed RSS (kB) |
| `shmem` | status `RssShmem` | shared-memory RSS (kB) |
| `swap` | status `VmSwap` | swapped-out (kB) |
| `hwm` | status `VmHWM` | peak RSS high-water mark (kB) |
| `vsize` | stat field 22 | virtual size (kB) |
| `utime`/`stime` | stat 14/15 | user/kernel CPU jiffies |
| `threads` | stat field 20 | live thread count |
| `minflt`/`majflt` | stat 10/12 | minor/major page faults |
| `rchar`/`wchar` | io | bytes read/written via syscalls |
| `rd`/`wr` | io `read_bytes`/`write_bytes` | bytes to/from block layer |
| `cwr` | io `cancelled_write_bytes` | writes cancelled before flush |
| `syscr`/`syscw` | io | read/write syscall counts |
| `vcs`/`nvcs` | status | voluntary/non-voluntary context switches |

A few fields need more than the table says:

- `t` is seconds since brokkr spawned the process, on the boot clock from a
  reading taken just before the spawn ("The clock" under "Marker & counter
  rules").
- `anon` is the usual "real memory" signal.
- `hwm` is carried monotonically.
- `utime` and `stime` decode through `_SC_CLK_TCK`.
- `minflt`, `majflt`, `rchar` and `wchar` are cumulative.

## Sample filters

These flags filter and project `--samples` output (and, where noted,
`--stat`):

- **`--fields rss,anon`**: project to a subset; JSON always keeps `t`.
- **`--where "majflt>0"`**: filter rows. Operators are `>`, `<`, `>=`, `<=`,
  `==` and `!=`, against any field in "Sample fields".
- **`--every N`**: keep every Nth row (downsample).
- **`--tail N` then `--head M`**: tail is applied first, so `--tail 100
  --head 10` is "last 100 rows, then the first 10 of those".
- **`--range 10.0..82.0`**: a seconds window.
- **`--phase FOO`**: restrict to a marker phase, by the name-resolution rules
  in "Marker views".
- **`--stat FIELD`**: prints min, max and avg, plus p50 and p95 as
  linear-interpolation percentiles.

A `--where` that doesn't parse, or a field name outside "Sample fields" in
`--where`, `--fields` or `--stat`, is an error before anything prints. It's
never a silently skipped filter or dropped column.

## Marker & counter rules

The marker FIFO has these line types, where `US` is the timestamp in
microseconds (see "The clock"):

- **Marker**: `US NAME`. Each gets a monotonic `marker_idx` in arrival order.
  The last name seen is also recorded, with the time it was seen, on the live
  hold's in-memory observation table. So `brokkr lock` shows it as `last
  marker: NAME (12s ago)` over the holder's control socket. The table entry
  is cleared whenever a FIFO is created or dropped, so an earlier run's marker
  in the same hold never stands in for the current run's. Markers are
  point-in-time bookmarks: the protocol itself knows nothing about spans or
  pairs. The name is untrusted text; every JSONL view encodes it with
  serde_json, so control characters are escaped.
- **Counter**: `US @NAME=VALUE`. The value must parse as `i64` or the line is
  rejected. A timestamp that doesn't parse is rejected for either type.
- **Call stream**: `US %CALL_ID NAME` and `US %CALL_ID @NAME=VALUE` are the
  same records in one call's stream, for a harness that runs several calls in
  one process. See "Call streams".
- **Sink end**: a sealing emission or terminal record in a tagged line. See
  "Sink ends".

### Call streams

A call id is 1 to 64 bytes of `A-Z a-z 0-9 _ . -`, so nothing is ever
escaped. It names one invocation within one process run, the producer keeps
it unique, and a human label belongs in the harness artefact, not the stream.
Lines without a tag are the process-wide stream.

A payload starting with `%` is only ever a tagged record. A bad call id, a
missing name or a bad counter is rejected, never read as an untagged marker,
so an untagged marker name can't start with `%`.

The producer writes each line, newline included, in one write of at most 512
bytes. The reader's own line limit is a defence, not that contract. A tag
says which invocation emitted a line, not which resources it measures: a
whole-process reading (mallinfo, page faults) stays whole-process when a
call's sink emits it.

### Sink ends

In a tagged record only, two body prefixes mark a call sink's end:

- **`@@`** is a sealing emission, `US %CALL_ID @@NAME=VALUE`, with a name of
  1 to 400 bytes without `=` and an `i64` value.
- **`!`** is a terminal record, exactly `US %CALL_ID !sealed drops=N` or `US
  %CALL_ID !failed drops=N`, where N is a `u64` in decimal, with no sign and
  no leading zero.

Any other tagged body starting `@@` or `!` is rejected. So a tagged counter
name can't start with `@`, and a tagged marker name can't start with `!`.
Untagged lines read as plain records: `US @@x=1` is a counter named `@x`.

A harness's call sink writes these when it's sealed. They're never markers or
counters in any view. sidecar.db stores them (`sidecar_sink_records`, since
its v10, a terminal's drops as exact decimal text), and the default summary
reads them. A run stored before v10 had no place for them and was stored as
incomplete, which it stays.

### The clock

Every timestamp on the FIFO is a non-negative integer count of microseconds
elapsed on the boot clock (`CLOCK_BOOTTIME`), which keeps counting through a
system suspend. Its zero is the start of the process brokkr spawned. Every
thread, call stream, counter and sink end shares that origin, and so do a
harness's call boundaries. Nothing resets it: not opening the FIFO, not
initialising instrumentation, not beginning a call, not the first record.
UNIX-epoch stamps don't conform.

The origin matters because brokkr stamps each `/proc` sample (the `t` field)
on the same clock, from a reading it takes just before spawning that
process. Every view that lines markers up against samples compares the two
directly: the per-phase summary, and `--phase` with `--samples`, `--stat` or
`--counters`.

The alignment is approximate. It includes the launch overhead between
brokkr's reading and the process start, and however the writer estimates
its own start. pbfhogg reads its process age from procfs, which counts on
the boot clock. A writer timing itself from a monotonic clock that stops in
suspend (Rust's `Instant`, `CLOCK_MONOTONIC`) conforms only while the host
stays awake: after a suspend its stamps fall behind the samples by the
suspend's length.

A writer that takes its origin later, at its first FIFO open or first record,
conforms only as far as that delay is negligible: every stamp is early
against the samples by it. Some readings don't depend on the origin:

- whatever only subtracts two stamps of one stream (`--durations`)
- `--stop` and `brokkr lock`'s last marker, which read names and brokkr's own
  observation time, not stamps

### Clock check

brokkr checks what it can. At the end of a capture, it counts any retained
record stamped negative, or later than the run interval plus one second, as
outside the run interval. The interval runs from brokkr's boot reading
before the spawn to the one at the child's observed end, so it includes any
suspend, as the stamps do; the wall, which excludes suspend, is not the
bound. When that end reading fails, brokkr warns and skips the check rather
than judge the stamps against the wall. It warns about outliers once and
stores the count: on the run's sidecar.db summary for a typed run
(`clock_outliers` and the smallest such stamp, since v11), and on the
child's evidence for a script-bench child. Every `brokkr sidecar` query of that run repeats the warning, one
line per run. The records themselves are stored and shown as written; nothing
is rebased.

Such a record is off the contract clock, or was written after the process
exited by a descendant still holding the FIFO. Passing proves nothing: a late
origin stamps early, never late, so it passes.

When the interval exceeds the wall by more than a second, brokkr warns once
that the boot clock ran ahead of the wall over the capture interval. A
system suspend is the usual cause, and the wall leaves it out. brokkr reads
the two clocks one after the other, so being descheduled between the reads
widens the gap as well. The child may also have exited before a suspend and
only been observed after it.

A run stored before v11 was never checked, and the run's wall is no longer in
sidecar.db. The upgrade marks one only for a stamp that's negative or at or
past 10^15 us, and leaves the rest unchecked. That threshold is 31.7 years,
an operational assumption that no measured process in the stored history ran
that long, not a protocol rule. Every UNIX-epoch stamp after 2001-09-09
exceeds it. No reader interprets or rebases a stamp by its magnitude. A
writer that stamps UNIX-epoch microseconds produces exactly such runs, at
capture and in the stored history alike.

### Arrival order and loss

The capture keeps every record in the one order it arrived in, markers,
counters and sink ends alike. sidecar.db stores each record's position in it
(`record_idx`, since v10; NULL on older rows, whose cross-kind order was
never recorded). Counters and sink ends read back in arrival order, and
markers in `marker_idx` order, the same order among markers. Runs stored by
the retired `brokkr library` also hold each call's boundaries and its sink's
own account (`sidecar_calls`), which still read.

Every rejected line warns and marks the run's stored trajectory incomplete,
so a later `brokkr sidecar` query still says so. That includes invalid UTF-8
and a record cut off without a newline when the writers close. A query that
finds no fault doesn't mean every line arrived: the FIFO is best effort, and
a harness's own end-of-call snapshot is the authoritative figure.

brokkr asks the kernel for a 1 MiB FIFO buffer, the unprivileged default
ceiling, so a producer between two drains drops fewer lines. Where that's
refused, the default stays.

### Streams in views

Every view groups markers and counters by `(run, call)` stream. `--call
CALL_ID` restricts a view to one call. A call id that the selected runs know
nothing of (no boundaries, markers, counters or sink ends) is an error.

A call with known boundaries splits its markers at its return for the default
summary and `--phase`. Those stamped up to `return_us` are its phases, the
last ending at the return. The later ones are its tail: points that aren't
timed call work. `--durations` pairs a call's `_START` and `_END` markers
over its whole stream: a span names its own two ends and needs no boundary.

Views that read one process timeline refuse `--run all`: the default summary,
`--stalls`, and any `--phase`. A `--phase` is resolved, and refused if it
doesn't resolve, before a view reports an empty result. With `--counters`, a
phase keeps only the counters of the stream it resolved in.

### Marker views

The marker protocol is convention-free. The sidecar timeline model applies
the conventions below to views that interpret marker names; `--markers`
still prints the raw events.

**Default summary and `--compare`** read the process stream only. Each marker
opens a segment running to the next marker, with no `_START` or `_END`
meaning. The last runs to just past the last sample, or is zero-length when
its marker is stamped after the last sample. That's a convention: the run's
wall isn't stored with its samples, so nothing observed bounds it, and
`--phase` on that marker gets the same interval. With no markers, the sampled
run is one `(all)` segment.

The default summary, never `--compare`, then lists each call stream: any call
with boundaries, markers or sink ends. Call rows have no resource columns,
since samples are process-wide. For a call with boundaries it prints:

1. a `type: call` record: entry, return and seal readings, its wall, its
   sink's account by its own document, the terminal record that arrived or
   `null`, and the count of sealing emissions
2. its phases (`type: call_phase`), the last ending at the return
3. its tail (`type: call_tail`)

Without boundaries, a call's last interval has no known end (`end_us:
null`). With `--call`, only that call prints. A run with no samples still
lists its calls.

**`--markers`** prints raw marker events in arrival order, each with its
`run` and, when tagged, `call`. It does no pairing or segment projection.

**`--phase FOO`** is resolved in one stream of one run, the process stream or
the `--call` stream, first match in time order:

1. an exact marker name, from that marker to the next
2. a completed `FOO_START`..`FOO_END` span
3. the first marker name containing `FOO`, from that marker to the next

For a call with boundaries, only its phases match, the last ending at the
return, and a name only its tail has is an error. Without boundaries, a match
on a call's last marker has no known end and is an error. With `--samples` or
`--stat`, `--call` needs `--phase`, and the samples stay process-wide.

**`--durations`** works per `(run, call)` stream: each `FOO_START` pairs with
the next later `FOO_END`. Unmatched starts and markers not used by a span
render as standalone points. Overlapping same-name spans in one stream pair
by that rule, which is deterministic but doesn't identify which operation
ended.

In `--human`, `--durations` collapses names seen more than once in a stream
into a single aggregate row, sorted by total descending, under a "Repeated
spans" heading. A row reads like `WAIT_P2_SEND x287  total 4200.0ms
min/avg/max`. The split is on observed cardinality, not on the name, so
nothing here knows what `WAIT_` means. Totals accumulate in microseconds, so
a crowd of sub-millisecond spans still sums correctly. Several streams print
under a heading each. JSONL keeps one record per span and standalone point,
with `run` and `call`.

**`--stop`** reads the process stream only: a call's marker never stops the
scenario. `FOO_END` matches verbatim, and `-FOO` resolves to `FOO_END`. Bare
`FOO` first checks for a marker named `FOO`, then falls back to `FOO_END`.
brokkr prints the resolved name whenever it differs from the requested
spelling.

Every chronological projection orders a stream's markers by timestamp, with
arrival order breaking ties, since a thread stamps a marker before it
competes for the write. The projections are segments, spans and phase
lookup.

### Counter views

Markers are reserved for the small set of true phase boundaries. A
high-frequency span in the marker stream drowns every phase-oriented view at
once, since each marker is a boundary. Accumulated blocking time is a counter
concept instead.

**`--stalls`** rolls up `*_wait_ns` counters per stream. It takes the max
value per name, which is the cumulative total since the counters are
strictly monotonic. The category is the name minus the `_wait_ns` suffix. It
reports:

- for the process stream, ms and `% of wall`
- for a call with boundaries, ms and `%` of the call's own wall (return minus
  entry)
- for any other call stream, ms alone, since the process wall would
  understate a short call

The observed max is a best-effort reading, not the call's total; a stored
library measurement's call rows have the call's authoritative snapshot. The
`%` can exceed 100 for waits accumulated across concurrent threads: it's the
average number of threads parked in that category, and single-threaded waits
read as clean sub-100%. One table covers both projects: it picks up
pbfhogg's `pipeline_*_wait_ns` and elivagar's `sort_chunk_write_wait_ns`,
among others.

**`--counters`** prints every counter point verbatim, with no aggregation:
`t=SEC name=value`, with `[call]` before the name when tagged. JSONL has
`run` and `call`. `--grep SUBSTR` keeps only counters whose name contains the
substring, and composes with `--phase`. Without it, a run emitting a progress
counter every 64 blobs buries the lines that matter.

The README's "Sidecar conventions" section has the emitter-side contract.

## Rendering

`brokkr sidecar` prints JSONL by default for every view, for machine and LLM
consumption; `--human` switches to fixed-width tables. The rendering follows
these rules:

- **The provenance header** goes to stderr, so it never contaminates the
  stdout JSONL. It has the run timestamp, PID, command, mode and dataset; the
  git commit and wall time; a non-zero exit code with its signal name (such
  as SIGKILL or OOM); and the recorded binary path and xxh128 with a live
  match check against disk.
- **The per-phase summary** (the default view) is a `summary` record, then
  one `phase` record per segment. The human table's columns are Phase,
  Duration, Peak RSS, Peak Anon, Peak Mflt, Disk Read, Disk Write and Avg
  Cores. An indented continuation line adds user and kern cores, majflt,
  minflt, vol_cs, nonvol_cs and peak_threads. Phases with zero in-phase
  samples (shorter than the 100 ms cadence) are dropped from the table but
  kept in JSONL with `avg_cores: null`.
- **Avg cores** decodes `utime+stime` jiffy deltas against
  `sysconf(_SC_CLK_TCK)`, read at runtime rather than assumed to be 100, over
  the phase's sample span. Too-short spans render `-` in the table and `null`
  in JSON.
- **Deltas are clamped to >= 0**, because cumulative `/proc` counters could
  regress if the process exits mid-read.
- **`--counters`** prints `t=SEC name=value` (human) or one JSON object per
  counter.

### Phase compare

`--compare` pairs the two runs' phases (the default summary's segments, best
run each) by ordinal within a name. The k-th segment named N on A pairs with
the k-th named N on B. Ordinals count in time order over every segment,
zero-length and sampleless ones included, so per-partition or per-pass
repeats compare in order.

The pairing is a correspondence convention, not identity recovery. A missing
emission shifts the later ordinals of its name, and same-name operations that
reorder pair different work.

Every segment of either run appears in exactly one row. A segment the other
side lacks (a surplus repeat, a name it never emitted) gets a one-sided row:
`(no match)` in the table, `null` in JSONL. Rows follow A's order. A B-only
segment sits after the row holding B's latest paired segment before it, or
first if there's none, in B order within that slot. So when pairs cross, the
B-only rows need not read in B's order.

`delta_pct` is B's duration against A's, from microseconds. It's `null`
(table `-`) unless both are positive. When any name repeats, the table adds a
`#` column with the ordinal, blank for a name that occurs once on each side
that has it.

In JSONL, `--compare` prints a `compare_summary` record (`a_uuid`, `b_uuid`,
`a_phases`, `b_phases`), then one `compare_phase` record per row (`name`,
`occurrence`, `a`, `b`, `delta_pct`). Each non-null side has `index`, its
zero-based position among that run's `type: phase` records in the default
JSONL view, and `start_us`.

## Run lifecycle

A sidecar run is sampled, can be stopped by a marker or a deadline, and can
be killed or interrupted. Each case behaves as follows.

### Sampling

Sampling cadence defaults to 100 ms and is injectable per sidecar run. The
shared supervisor schedules each tick against monotonic time, so the roughly
30 us of `/proc` read overhead doesn't accumulate drift. Each tick reads
`/proc/PID/stat`, `/proc/PID/io` and `/proc/PID/status`.

The wall is the exit, not a tick. Between ticks the supervisor waits on the
child's pidfd, so a natural exit wakes it at once and the wall is taken
there. A short run's wall isn't rounded up to the next multiple of the sample
interval. The exit wake still drains the FIFO, so a stop marker written
before the exit still wins. It takes no `/proc` sample, so no sample
postdates the wall. Where a pidfd can't be opened, the exit is found on the
next tick.

Rows recorded before pidfd exit detection have the tick-quantized wall. It's
up to one sample interval (100 ms by default) late, always upward, and most
visible on short single-iteration runs.

### Stop markers

A stop marker SIGKILLs the whole child process group, so descendants are
reaped too, the moment the marker arrives. That SIGKILL isn't a failure: the
run is flagged as stopped by marker and records normally, so you can bench
one phase in isolation. A script bench asks for one per call (`brokkr man
bench`).

A typed results row kept two facts apart: `stop_marker`, the marker that
actually fired, shown as `killed at "..."`, and `stop_marker_requested`,
the `--stop` spelling as given. A requested stop whose marker never
appeared left `stop_marker` empty. Rows recorded before schema v20 only ever
stored the request. Their `stop_marker` was moved to `stop_marker_requested`
and cleared, so whether those stops fired is unknown.

### Deadlines

A caller may bound each child's wall time, and the capture helpers pass the
bound through. Past it, the child's process group is SIGKILLed, the run is
flagged as stopped by deadline, and the run fails.

The deadline bounds the child's whole life, a stop marker included. A child
still running when it passes is killed as overdue even with a stop marker
queued, and a stop's SIGTERM grace ends at it. Paths with no deadline run until the child exits or
`brokkr kill`.

### Graceful kill

`brokkr kill` asks the holder to stop over its control socket, when the
holder is in the kill's scope. That's the caller's project unless `--project
NAME` or `--all` says otherwise, and queued commands in scope are cancelled
first (see `brokkr man hygiene kill`).

Every fresh hold binds `~/.brokkr/lock/ACQUISITION_ID.sock`, the id recorded
in the lock file. The holder itself accepts a stop request naming the live
acquisition. It sets a sticky stop flag that no guard clears and that refuses
every further lock acquisition and build admission. It answers `accepted`,
then sends itself SIGTERM. The answer goes first, because outside a guard
that SIGTERM kills the holder at once. "Accepted" means that commitment, not
completed termination.

Because nothing crosses a PID namespace, this works from the host against a
sandboxed holder and between sibling sandboxes. Codex and Claude Code run
each command in its own PID namespace.

A request that was delivered but got no well-formed answer is reported as
unconfirmed. `kill` watches the lock for a few seconds and says whether the
holder let go. It never follows up with a signal of its own, since a second
SIGTERM would escalate a holder already handling the first.

Only when the request never reached the socket (a missing socket, say) does `kill` fall back to a numeric SIGTERM. It does so only when the
holder's recorded PID and time namespaces match the caller's, and `/proc`
numbers processes for the caller's own namespace.

`brokkr lock` asks the same socket for the holder's own account of itself:
invocation, how long it has held, its own CPU and RSS, run progress, its open
activities, its processes and the last sidecar marker (see `brokkr man
hygiene lock`). Both it and the lock wait print an advisory line when the
holder recorded the caller's own `CODEX_THREAD_ID`.

### Interrupts

A signal guard catches SIGTERM and Ctrl-C (SIGINT) for every tracked-child
window:

- each sidecar run
- a passthrough child
- every cargo build
- the whole of `check`, `test`, `clippy` and `deps`
- the other commands that run captured children with no sidecar: `visual`
  and `prepare`

A loop over such children stops at the first `Interrupted` rather than
recording it as that step's failure.

Guards nest. The outermost installs and restores the handlers, so an inner
guard (the sidecar's, inside a script bench's) can neither clear a request
that already arrived nor uncover the outer window when it drops. A second
signal while a request is still pending takes the default action, so a wait
that never polls the flag can't swallow the interrupt. Outside every guard,
SIGTERM terminates brokkr by default, except that registered test process
groups are SIGKILLed first.

On a catch in the sidecar window, brokkr kills the child and keeps the
partial sidecar data with the run. The run returns `Interrupted`, the runner
discards the attempt's own scratch output, and brokkr exits 130 (`brokkr man
clean after-an-interrupt`).

### Hard kill

`brokkr kill --hard` runs no handler. It SIGKILLs an in-scope holder's
recorded child and every identity-checked descendant of
brokkr, then brokkr. Nothing is flushed; follow up with `brokkr clean`.

It signals by PID, so it refuses unless the holder shares the caller's PID
and time namespaces, the same check as the SIGTERM fallback. Against a
sandboxed holder, use the graceful `kill` or stop the session that owns it.
Delivery failures (EPERM) are reported, never counted as sent, and fail the
kill once every other target has had its signal.

Known gaps: a process group is signalled with `kill(-pid)` after a starttime
re-check, and starttime is a clock-tick token, not a unique generation
number. A brokkr SIGKILLed by anything else (the OOM killer) gets no walk at
all. Only the test runners' processes have a backstop for that case: a
parent-death signal, and the orphan reap at the next locked command (see
`brokkr man hygiene strays`).

### Crash preservation

Sidecar data is kept even when the child is OOM-killed, segfaults, or exits
non-zero: the `/proc` trajectory up to the kill is the whole point. This
holds on every sidecar-observed child, profiled ones included.

The child is also marked as the kernel OOM killer's preferred target, so a
memory blow-up takes the benchmarked process, not the host.

## Capture limits

Samples, markers, counters and FIFO records have explicit limits. The sample,
marker and counter budgets apply across all iterations in one measurement, so
a large iteration count can't multiply the retained trajectory without
bound. Overlong FIFO records are discarded through their newline without
growing the temporary line buffer.

Reading the FIFO is bounded too. Each supervisor tick consumes at most 4 MiB
of it, discarded bytes included, so a child writing without pause can't hold
off its deadline or a stop. A record the budget cuts resumes on the next
tick. After the child ends, one last drain reads at most 16 MiB. A child's
descendant still writing past that leaves records possibly unread, which
counts as an omission like the limits above.

A sealing emission counts against the counter limit and a terminal record
against the marker limit. A FIFO read error is an omission too.

When anything is omitted, the run warns naming each cause:

- rejected lines
- records possibly unread
- the marker or counter limit
- the sample limit
- cut child output

It also stores a truncation flag with that iteration's sidecar data, and
querying an affected run with `brokkr sidecar` warns that the stored
trajectory is incomplete.

### Child output

The shared child supervisor also bounds captured stdout and stderr, per
stream:

- 16 MiB for ordinary commands and sidecar measurements
- 128 MiB for Cargo output

The `[cargo_env]` evaluator and `script_check` have smaller
contract-specific limits. The supervisor keeps reading after a limit, so the
child can't block on a full pipe. A captured command that exceeds its limit
fails with a capture error rather than returning partial bytes to a result
parser. A sidecar measurement keeps the bounded bytes, marks its trajectory
incomplete, and fails that iteration when a child stream exceeds its limit.

## Hotpath JSON

brokkr doesn't depend on the `hotpath` crate directly. It parses the JSON
report that hotpath-instrumented binaries write to `HOTPATH_OUTPUT_PATH`:

- Function percentiles are read from every `pN` field (`p` and a number),
  sorted numerically, and may be fractional (`p99.9`).
- Timing values are stored in nanoseconds, byte values in bytes, and
  percentages as percentages.
- Function diffs show a relative change only when both totals have the same
  quantity kind. A duration is never compared with an allocation size or
  percentage.
- The results database keeps the old display-text columns and parses them
  when a typed metric is missing. Existing rows aren't rewritten.

brokkr sets these env vars on hotpath child processes:

- `HOTPATH_METRICS_SERVER_OFF=true`
- `HOTPATH_OUTPUT_FORMAT=json`
- `HOTPATH_OUTPUT_PATH`, a report path under the project's `.brokkr/`,
  removed after the launch

## Refusals

The table lists what `brokkr sidecar` refuses, with the text brokkr prints
after `[error]`. Words in capitals stand for values brokkr fills in. What a
script bench refuses is in `brokkr man bench`, and the lock's own refusals,
a brokkr started beneath the lock holder included, are under "Refusals" in
`brokkr man hygiene`.

| Message | Cause | Fix |
|---|---|---|
| `config: uuid prefix 'PREFIX' is ambiguous - it matches N runs:` | The prefix names several sessions, typed or script bench (see "Sidecar queries"). | Pass a longer prefix from the listed candidates. |
| `config: empty uuid prefix would match every row - give at least one character` | The UUID argument was empty or blank. | Pass at least one character of the run id. |
| `script-bench run ID belongs to project NAME` | The prefix names only another project's bench run. | Run `brokkr sidecar` from that project. |
| `usage: --child selects a child of a script-bench run's iteration; ID is a typed run` | `--child` was given for a typed run, which has one process. | Drop `--child`. |
| `config: bench run ID: ...` | The selected iterations launched several children without `--child`, or fewer than `--child` names (see "Selecting a child"). | Pass `--child N`, or a `--run` that launched the child you want. |
| `usage: --run N: bench run ID has N iterations` | `--run` names an iteration the run doesn't have. | Pass an iteration the run has, or `--run all`. |
| `usage: VIEW reads one run; pass --run N instead of --run all` | The default summary, `--stalls` and `--phase` read one timeline (see "Streams in views"). | Pass `--run N`. |
| `config: --phase: no marker matching 'NAME'. Available:` | The phase doesn't resolve in the selected stream (see "Marker views"). | Pass one of the listed names. |
| `config: --phase: 'NAME' matches only markers call 'ID' emitted after it returned` | The name is only in the call's tail. `resolves to the last marker of call` is the same refusal for a call with no known end. | Pick a phase inside the call's timed work. |
| `config: --call: no call 'ID' in the selected runs` | The selected runs know nothing of that call. | Pass a call id the run recorded. |
| `usage: --call: 'ID' is not a call id` | The id breaks the call id grammar (see "Call streams"). | Pass 1 to 64 bytes of `A-Z a-z 0-9 _ . -`. |
| `usage: --call with --samples or --stat needs --phase` | Samples are process-wide, so a call needs a phase to give a window. | Add `--phase NAME`. |
| `config: FLAG: unknown sample field 'NAME'` | `--fields`, `--where` or `--stat` named a field outside "Sample fields". | Use a field from the listed ones. |
| `config: --where: invalid condition 'COND'` | The condition doesn't parse (see "Sample filters"). | Write it as `FIELD OP INTEGER`, such as `majflt>0`. |
| `config: --range: expected 'start..end' in seconds` | The range doesn't parse. | Write it as `10.0..82.0`. |
| `config: --compare: bench run ID has no result; only a run with a result compares` | A bench side has no result iteration (see "Child compare"). | Compare a run that recorded a result. |
| `usage: --compare: 'OPERAND' has more than UUID/ITER/CHILD` | The operand has too many parts, or a non-number part. | Write `UUID`, `UUID/ITER` or `UUID/ITER/CHILD`. |
