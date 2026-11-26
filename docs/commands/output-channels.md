# Output channels

When brokkr runs a project binary under a measurement mode, the binary's
output can go to your terminal, to the results DB, to the sidecar DB, or
nowhere at all. Which of these happens depends entirely on which harness path
the command's dispatch chose; the project binary itself never knows what mode
it runs under. "Databases" and "Console output" describe where output is
kept and shown, "Transport channels" and "Capture by mode" are the reference
map, and "Choosing a channel" says which one to use.

## Databases

brokkr keeps two gitignored databases per project:

- **`.brokkr/results.db`**, kept small. It holds one row per bench run:
  `command`, `elapsed_ms`, `commit`, `input`, and a `kv` column of arbitrary
  `key=value` metrics. Query it with `brokkr results`.
- **`.brokkr/sidecar.db`** holds per-run `/proc` sample trajectories, phase
  markers and application counters. For a harness that runs calls in one
  process, it also holds each call's sink ends and boundaries. Query it with
  `brokkr sidecar UUID`.

brokkr records two things in results.db itself rather than receiving them
over a transport channel. These are the per-iteration walls of a `--bench N`
run (`run_iterations`, in execution order) and the `prev.*` kv pairs naming
the previously recorded run. Neither comes from the binary; see `brokkr man
measure iteration-walls` and `brokkr man measure prev-fields`.

Both databases live under the project root's `.brokkr/`, never a `--commit`
worktree's. brokkr resolves them identically for every project and reads git
provenance from the build root. `brokkr man measure sidecar` has the sidecar's
sampling and marker protocol.

A script bench's children (`brokkr man bench`) are captured the same way but
stored in the per-user bench store instead, each child's records apart.
`brokkr results` and `brokkr sidecar UUID` read them there.

The `.brokkr/` directory ignores itself. Every command that reads config
creates it with a `.gitignore` of `*`, left alone if one is already there.
So its logs, journals and databases stay out of `git status` and out of a
`git add -A`, even in a repo whose own `.gitignore` never mentions it. A
project that commits its `results.db` keeps doing so: git keeps tracking a
tracked file whatever an ignore rule says, and the first add is `git add -f`.

## Console output

brokkr prints its own lines around a measured run: the `run i/n` line, the
single `[sidecar]` summary, the `[run] results.db ID` line naming the stored
row, and the `[run] commit` line for `--commit`. `brokkr man measure
run-output` specifies them.

The prefixed printers (`[build]`, `[run]`, `[lock]`, `[strays]`, `[guard]`,
`[bench]`, `[verify]`, `[sidecar]`, `[dry-run]`, ...) all go through one
renderer, and none writes to the console directly. The renderer tees each
line into the run log when the command keeps one, and keeps a drawn status
line intact. The commands that keep a run log are `check`, `test`, measured
runs and `crit`, writing `.brokkr/check-logs/KIND-MS.log`.

Each prefix is padded to ten columns, and a tag of ten or more
(`[download]`) is followed by one space instead of being cut. The prefixes mean:

- **`[lock]`**: lock events
- **`[strays]`**: the stray-process reaps and `brokkr strays`
- **`[guard]`**: `brokkr guard`
- **`[dry-run]`**: a planned action under `--dry-run`

Lines go to stdout, with these exceptions on stderr:

- `[sidecar]` narration, so `brokkr sidecar ... | jq` stays clean
- the deliberately bounded `[warn]` bookkeeping paths, which attempt both
  logging and rendering; they use stderr so a final JSON summary remains the
  last stdout line
- sidecar provenance errors and warnings

Quiet mode hides progress on the console but keeps it in the run log.
Warnings, errors, requested test verdicts and lock, guard and stray actions
stay visible. The output channel owns all `[result]` framing. An
unretrievable result (a dirty run or a failed database insert) uses the same
printer without quiet suppression. So does the `[sidecar]` line naming a
session no results row names. Non-fatal bookkeeping failures are warnings.

JSON, query tables, outline and other data output keep their exact bytes and
streams. Formatting for operator prose doesn't touch data:

- Human-readable durations appear in operator prose only, never in stored
  metrics or table formats.
- A path inside the project prints relative to the directory brokkr was
  invoked from in operator lines and in the run log, whose header records
  that directory. Stored values, argv and data output keep it absolute.

A captured child's output isn't shown while the child runs. Where brokkr
shows it, it replays the captured bytes after the child exits.

## Transport channels

A running binary can hand data to brokkr three ways: stdout, stderr
`key=value` lines, and FIFO markers and counters.

### stdout

stdout is human-readable prose. It's either inherited (printed straight to
the terminal) or captured (drained into a buffer and dropped). brokkr never
parses or stores it.

### stderr kv

stderr `key=value` lines are structured metrics, parsed on every path that
scrapes stderr. A line counts only if the text before its first `=`, trimmed,
is a **field name**: an ASCII letter or `_`, then ASCII letters, digits, `_`,
`.` or `-` (`rows`, `cold_prepare_us`, `cache.hit-rate`).

Any other line is prose and is skipped silently. So `Note: auto-selected
index (sorted=true)` or `time-filter: N versions scanned, M <= cutoff` store
nothing. The value is the trimmed rest of the line, `=` included (`label=a=b`
stores `a=b`). The grammar rejects nonconforming names, but it doesn't detect
prose. So a sentence that opens with a field name and `=` (`wheel = zoom,
drag = pan`) still reads as a field.

These names are special on every scraping path:

- `elapsed_ms` and `total_ms` are reserved timing names, never stored as
  counters.
- A field in one of brokkr's own namespaces (`meta.`, `env.`, `prev.`,
  `threads.`, `sidecar.`, `provenance.`) is dropped, with one warning per
  key. Those keys record what brokkr observed, so a child can't write them.

A child's own timing line is never the recorded wall: every measured path
times the child itself, best-of-N on brokkr's wall clock, and skips the
reserved names.

Every other field's value is type-inferred in order: `i64`, then finite
`f64`, then text. A non-finite float like `NaN` or `inf` falls back to text.
Those pairs go into the results.db `kv` column and the `[result]` line.

Rows stored before the field-name rule keep whatever keys the older scrape
gave them, prose-shaped ones included. A `--compare` against such a row lists
those as counters present on one side only.

### FIFO

FIFO markers and counters go through `BROKKR_MARKER_FIFO`. The target writes
one of these line formats, where `US` is microseconds since the process
started:

- `US NAME`, a marker
- `US @NAME=VALUE`, a counter; the value must parse as `i64`
- `US %CALL_ID ...`, either of the above tagged with a call stream, which
  also has a call sink's sealing and terminal records

"Marker & counter rules" in `brokkr man measure` has the full rules, "The
clock" included. brokkr drains the FIFO into one arrival-ordered capture,
then into sidecar.db. Any project binary can emit these by writing that line
format to the FIFO.

In pbfhogg, markers and counters go to the invocation's instrument sink. The
CLI opens the FIFO as that sink from `BROKKR_MARKER_FIFO`, and passes no sink
when the variable is absent (no sidecar attached).

## Derived result fields

The `[result]` line adds fields brokkr computes itself, on top of the raw kv
pairs:

- **`read_mbs`** appears whenever the input size is known and the wall is
  non-zero. The input size is `config.input_mb`, resolved from the dataset
  config. The value is `input_mb / secs`, with `secs` from `elapsed_us` when
  the run has it (so a run that rounds to 0 ms still gets a figure) and from
  `elapsed_ms` otherwise.
- **`write_mbs`** appears additionally when the run reported an
  `output_bytes` kv pair, so this one does depend on the binary emitting it.
  The value is `output_bytes/1e6 / secs`.

So a metric can reach the `[result]` line three ways:

- a raw stderr kv pair
- a brokkr-derived throughput field
- for `write_mbs`, a kv pair that feeds a derived one

The external wall-clock path also scrapes counters; only its timing comes
from brokkr rather than stderr.

## Capture by mode

Each measurement mode captures a different set of channels. The table lists
what each keeps:

| Mode | stdout | stderr kv | FIFO |
|---|---|---|---|
| run mode, no `--bench` | shown | no | no FIFO |
| `brokkr mogwai` | dropped | counters only | yes |
| `--hotpath`, `--alloc` | captured | see below | yes |

The columns read as follows:

- **stdout**: shown means inherited; dropped means captured and dropped.
- **stderr kv**: what goes to results.db. `--hotpath` and `--alloc`, in
  every project, keep stderr counters and a JSON profile.
- **FIFO**: whether markers and counters reach sidecar.db.

The external path, `brokkr mogwai`, measures the child from outside. It
records the exact duration in microseconds in `elapsed_us`, and
`elapsed_ms` is that value rounded to nearest.

Missing or invalid hotpath JSON fails the iteration and preserves its
sidecar under `dirty`, rather than recording a profile mode without a
profile.

The external path scrapes non-timing stderr `key=value` pairs for counters,
because stderr is captured either way and the scrape is free. A wall on its
own can't distinguish "the code got faster" from "the code did less". The
counters answer that, and they drive `--compare`'s `counters:` line. The
reserved `elapsed_ms` and `total_ms` keys are omitted from the counters.

Two consequences follow:

- **Run mode (no `--bench`) stores nothing and attaches no sidecar.** stdio
  is inherited, so the binary's own prose reaches the terminal verbatim. FIFO
  emission has nowhere to go, because `BROKKR_MARKER_FIFO` is unset.
- **In every captured path, brokkr swallows the binary's stdout prose** and
  prints its own `[result]` line instead. To survive bench mode, data must go
  out as a FIFO counter or, on the kv path, an stderr `key=value` line.

## Choosing a channel

Pick the channel by where you want the metric to show up:

- **In `brokkr results`**: print a non-timing `key=value` line to stderr, its
  key a field name with nothing before it (see "stderr kv"). On the external
  wall-clock path (`brokkr mogwai`), it's scraped into `kv` as a counter and
  feeds `--compare`'s `counters:` line. There, `elapsed_ms` and `total_ms`
  are reserved and ignored, and the wall stays brokkr's own.
- **In `brokkr sidecar`**: emit an `@NAME=VALUE` counter to
  `BROKKR_MARKER_FIFO`. This works on any path that spawns the sidecar, in
  bench, hotpath and alloc modes.
- **Only on your terminal**: print to stdout and run without `--bench`.
