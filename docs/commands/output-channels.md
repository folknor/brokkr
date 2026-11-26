# Output channels

When brokkr measures a project binary, the binary's output can go to a
spool, to the sidecar, or nowhere at all. Every measuring path is a script
bench (`brokkr man bench`), and the project binary itself never knows that it
is measured. "Stores" and "Console output" describe where output is kept and
shown, "Transport channels" is the reference map, and "Choosing a channel"
says which one to use.

## Stores

A script bench's runs go to the per-user bench store, each child's records
apart. `brokkr results` and `brokkr sidecar UUID` read them there.

Two gitignored databases per project hold the typed history from before
script benches, read-only (`brokkr man measure stored-history`):

- **`.brokkr/results.db`** holds one row per typed run: `command`,
  `elapsed_ms`, `commit`, `input`, and a `kv` column of the `key=value`
  metrics the run scraped from its child's stderr. Query it with
  `brokkr results`.
- **`.brokkr/sidecar.db`** holds those runs' `/proc` sample trajectories,
  phase markers and application counters. Query it with
  `brokkr sidecar UUID`.

Both live under the project root's `.brokkr/`, never a `--commit`
worktree's. `brokkr man measure sidecar` has the sidecar's sampling and
marker protocol.

The `.brokkr/` directory ignores itself. Every command that reads config
creates it with a `.gitignore` of `*`, left alone if one is already there.
So its logs, journals and databases stay out of `git status` and out of a
`git add -A`, even in a repo whose own `.gitignore` never mentions it. A
project that commits its `results.db` keeps doing so: git keeps tracking a
tracked file whatever an ignore rule says, and the first add is `git add -f`.

## Console output

The prefixed printers (`[build]`, `[run]`, `[lock]`, `[strays]`, `[guard]`,
`[bench]`, `[verify]`, `[sidecar]`, `[dry-run]`, ...) all go through one
renderer, and none writes to the console directly. The renderer tees each
line into the run log when the command keeps one, and keeps a drawn status
line intact. The commands that keep a run log are `check`, `test`,
`brokkr bench` and `crit`, writing `.brokkr/check-logs/KIND-MS.log`.

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
stay visible. Non-fatal bookkeeping failures are warnings.

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

A running binary can hand data to brokkr two ways: its output streams, and
FIFO markers and counters.

### stdout and stderr

A script bench spools a child's stdout and stderr in full to the iteration
directory, and hands the script a bounded preview of each (`brokkr man
bench`). brokkr parses neither: a metric the child prints is the script's to
read and record.

Rows the typed history stored kept `key=value` pairs scraped from a child's
stderr. A field in one of brokkr's own namespaces (`meta.`, `env.`, `prev.`,
`threads.`, `sidecar.`, `provenance.`) was dropped there, and `elapsed_ms`
and `total_ms` were reserved timing names, so neither appears among a row's
counters. Rows stored before the field-name rule keep whatever keys the older
scrape gave them, prose-shaped ones included. A `--compare` against such a
row lists those as counters present on one side only.

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
stored with the run. Any project binary can emit these by writing that line
format to the FIFO.

In pbfhogg, markers and counters go to the invocation's instrument sink. The
CLI opens the FIFO as that sink from `BROKKR_MARKER_FIFO`, and passes no sink
when the variable is absent (no sidecar attached).

## Choosing a channel

Pick the channel by where you want the metric to show up:

- **In `brokkr results`**: have the bench script record it from the child's
  output or an output file (`brokkr man bench`).
- **In `brokkr sidecar`**: emit an `@NAME=VALUE` counter to
  `BROKKR_MARKER_FIFO`. This works on every child a script bench measures,
  in timing, hotpath and alloc runs.
- **Only on your terminal**: print to stdout and run the binary with
  `brokkr run`.
