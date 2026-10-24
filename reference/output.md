# Console output contract

`src/output.rs` exposes the console channel and the child supervisor. Operator
messages use the console printers. The channel owns prefix framing, run-log
teeing, clearing and redrawing the terminal status line, and quiet filtering.
`output::result_msg` and `output::result_forced` share one result renderer.
Quiet progress still reaches an open run log; forced results remain visible
when the result cannot be retrieved from storage.

Operator messages default to stdout. Sidecar query provenance, including its
errors and warnings, uses stderr so query data on stdout stays parseable. A
command whose stdout is a value (`brokkr build`, whose one line is a path)
calls `console::reserve_stdout` at startup, before the lock is taken: from then
on every prefixed line, raw replay and status line goes to stderr, and
`console::data_stdout` is the only writer left on stdout.
Late bookkeeping warnings use `output::warn_forced` on stderr to leave a final
JSON trailer intact. Shutdown warnings and errors attempt the log and renderer
with bounded mutex waits rather than waiting indefinitely. A busy run log is
skipped; a busy renderer is bypassed with a direct stderr write for
`warn_forced` (the warning still reaches the user), while `error_forced` skips
it. This exception is for shutdown and finalization, not routine
narration. Warnings and errors are never quiet-suppressed.

Genuine data output preserves its existing stream and bytes. The crate denies
Clippy's `print_stdout` and `print_stderr`; function-scoped exceptions cover
JSON, tables, outline and subprocess protocols. Such exceptions do not permit
operator narration. Textlint also rejects hand-written operator prefixes and
warning tags. Raw captured replay uses `output::raw_stdout` or
`output::raw_stderr`, which add no newline and use the renderer and log.
`output::run_streamed` replays selected pipe chunks through the channel as they
arrive, while retaining the supervised capture and ending contract. Nidhogg
ingest replays stderr only; update replays both streams. Byte-oriented replay
preserves split UTF-8 sequences and non-UTF-8 bytes. Inherited child streams
remain owned by the child.

Operator elapsed durations use `output::format_duration`: milliseconds below
one second, tenths of a second below one minute, whole minutes/seconds below
one hour, then hours/minutes. `format_duration_ms` adapts stored millisecond
counts. Numeric measurement fields, data tables and the positive-integer
frontmatter duration grammar are separate wire formats. Allocation-profile
caveats are emitted by `measure::ProfiledMode::print_note`.

Unknown cargo diagnostic levels are schema extensions: they are skipped as
findings, with the level and stdout line number recorded through
`output::detail` in an open run log. Cargo exit status still determines failure.
