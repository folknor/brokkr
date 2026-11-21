# Console output

This page is the contract for operator output: the lines brokkr prints about
what it is doing, as opposed to the data a command produces. The console
channel and its streams come first, then shutdown warnings, data output,
durations, paths and cargo diagnostic levels.

## Console channel

`src/output.rs` exposes the console channel and the child supervisor, and
operator messages use the console printers. The channel owns:

- prefix framing
- run-log teeing
- clearing and redrawing the terminal status line
- quiet filtering

`output::result_msg` and `output::result_forced` share one result renderer.
Quiet progress still reaches an open run log. Forced results stay visible
when the result can't be retrieved from storage.

`output::sidecar_forced` applies the same rule to a sidecar session with no
results row. The line naming its id is the session's only handle, so brokkr
never quiet-suppresses it. Warnings and errors are never quiet-suppressed
either.

## Streams

Operator messages default to stdout. Sidecar query provenance, including its
errors and warnings, goes to stderr so query data on stdout stays parseable.

Some commands have a stdout that is a value (`brokkr build`, whose one line
is a path) or that belongs to a program they run (`brokkr run`). Such a
command calls `console::reserve_stdout` at startup, before the lock is
taken. From then on:

- every prefixed line and raw replay goes to stderr
- the transient status line is off
- `console::data_stdout` is brokkr's only writer left on stdout

A child with inherited streams still writes where it likes.

## Shutdown warnings

Late bookkeeping warnings use `output::warn_forced` on stderr, so a final
JSON trailer on stdout stays intact. Shutdown warnings and errors try the log
and the renderer with bounded mutex waits rather than waiting indefinitely.

When one of those mutexes is busy:

- a busy run log is skipped
- a busy renderer is bypassed with a direct stderr write for `warn_forced`,
  so the warning still reaches you
- `error_forced` skips a busy renderer

This exception is for shutdown and finalization, not routine narration.

## Data output

Genuine data output keeps its existing stream and bytes. The crate denies
Clippy's `print_stdout` and `print_stderr`, and function-scoped exceptions
cover JSON, tables, outline and subprocess protocols. Those exceptions don't
permit operator narration. Textlint also rejects hand-written operator
prefixes and warning tags.

Raw captured replay uses `output::raw_stdout` or `output::raw_stderr`, which
add no newline and use the renderer and the log. No captured stream is
replayed while its child runs. Inherited child streams stay owned by the
child.

## Durations

Operator elapsed durations use `output::format_duration`, which prints:

- milliseconds below one second
- tenths of a second below one minute
- whole minutes and seconds below one hour
- hours and minutes above that

`format_duration_ms` adapts stored millisecond counts. Numeric measurement
fields, data tables and the positive-integer frontmatter duration grammar are
separate wire formats. `measure::ProfiledMode::print_note` emits the
allocation-profile caveats.

## Paths

Operator lines name a location with `output::path`, never `Path::display`. A
path inside the project tree prints relative to the invocation directory
(`.brokkr/...`, or `../.brokkr/...` when `brokkr.toml` sits one level up).
Every other path prints absolute.

The decision is lexical over path components:

- containment is by spelling
- a remainder with `..` stays absolute
- a leading `-` or `~` gains `./`

The printers never rewrite rendered text. A tagged line can have captured
child output or opaque values that only look like paths, so the caller that
holds a `Path` it means as a location is the one that asks. Argv, stored
values, JSON and other data keep the absolute spelling.

`DevError` text is operator text and uses `output::path`. Where an error's
text becomes a stored value, brokkr renders it inside
`output::with_absolute_paths`, which ignores the anchor for that thread while
it runs. Stored error text includes a script-bench run's failure reason and
the unreadable reason of a capture or a stderr tail. A new store of error
text must do the same.

The anchor is fixed once at startup. Before that, and in unit tests, every
path prints absolute. The run log's own header records the absolute
invocation cwd, so its relative lines stay resolvable.

## Cargo diagnostic levels

brokkr treats an unknown cargo diagnostic level as a schema extension. It
skips such a diagnostic as a finding and records the level and stdout line
number through `output::detail` in an open run log. Cargo's exit status still
decides failure.
