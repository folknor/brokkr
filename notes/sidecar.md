# Sidecar enrichment - scoped plan

Status: **design for immediate implementation.** Supersedes the earlier
`sidecar-mvp.md` and `sidecar-protocol-v2.md` drafts. Both of those
tried to change the emission contract (new FIFO line discriminators,
span IDs, typed counters, new tables, metric metadata) before we had
evidence the existing contract was the bottleneck.

This doc scopes the work to **reporting-layer enrichment of data we
already capture**, plus two convention-only additions pbfhogg opts into
at its own pace. No protocol changes, no schema changes, no
co-migration with a real ALTW benchmark.

## What we already have

- `/proc/{pid}/stat|io|status` sampled at 100 ms → `sidecar_samples`
  table. Every field we need for per-phase CPU/fault/context-switch/
  thread-count analysis is already stored (`utime`, `stime`, `minflt`,
  `majflt`, `vol_cs`, `nonvol_cs`, `num_threads`).
- FIFO markers with user-assigned names → `sidecar_markers`.
- FIFO counters keyed by name → `sidecar_counters`.
- Per-phase summary view that pairs markers by `_START`/`_END` suffix
  and computes duration / peak RSS / disk I/O / average cores.

## What this adds

### 1. Per-span `/proc` enrichment in the phase summary

Extend the existing `PhaseSummary` struct and the phase-summary JSONL /
human-table renderers with fields derived from data already in
`sidecar_samples`:

- `user_cores` = Σ utime_delta / sample_span
- `kernel_cores` = Σ stime_delta / sample_span
- `majflt_delta`, `minflt_delta` (full per-phase deltas; `peak_majflt`
  is already there for single-sample spikes)
- `vol_cs_delta`, `nonvol_cs_delta`
- `peak_threads` (max `num_threads` across in-phase samples)

Zero emitter changes. Zero schema changes. Works on every historical
run that has sidecar samples.

### 2. Truncation / dropped-event visibility

- **Truncated spans.** Unpaired `_START` markers at child exit (marker
  open with no matching `_END`) render in the phase summary with
  `status=truncated` and the phase ends at the child's exit timestamp.
  Today we silently drop them.
- **Dropped events.** When FIFO writes fail (buffer full, O_NONBLOCK),
  the sidecar already knows. Surface the count as a run-level field in
  the detail view - a non-zero value means the marker/counter trace is
  partial. If the sidecar doesn't currently track this, add a single
  `dropped_events` counter to `SidecarData` and surface it in the
  `print_run_info` block.

### 3. `--stalls` view (convention-only)

```
brokkr sidecar <uuid> --stalls
```

Sums durations of spans (marker pairs) whose name begins `WAIT_`,
grouped by the prefix-stripped category, expressed as a fraction of
the enclosing top-level span's wall-clock time. Pure query over
existing markers - brokkr doesn't validate the naming; any span named
`WAIT_FOO` is treated as a stall in category `FOO`.

Pbfhogg adopts the convention at its own pace, for the blocking points
that matter to the ALTW rewrites. No migration pressure; old runs
won't have `WAIT_*` spans and `--stalls` on those reports "no WAIT_*
spans in this run" rather than empty output.

### 4. `--stop FOO` / `--stop -FOO` aliases

`--stop FOO_END` keeps working verbatim (backward compat; lots of
existing scripts use the suffix form). Add two aliases:

- `--stop -FOO` → matches the end marker of a span named `FOO`
  (resolves as `FOO_END` today, reserved for future span-close
  semantics).
- `--stop FOO` → first tries a literal match, then falls back to
  `FOO_END`. Prints a one-line note when the fallback fires so users
  see the resolved form.

This is a string-level convenience, not a protocol change. Lets
pbfhogg invocations drop the `_END` noise without brokkr needing to
know anything about span semantics.

## Handling older rows

Pre-convention rows will:

- Work fine for per-span enrichment (the /proc samples have been
  captured for months; this is purely an analysis-layer addition).
- Render `--stalls` as "no `WAIT_*` spans in this run" with a pointer
  at the conventions doc.
- Work with both `--stop FOO_END` (verbatim) and `--stop FOO`
  (falls back to `FOO_END`).

Truncated-span detection also works retroactively - any historical
unpaired `_START` becomes a visible `status=truncated` entry in the
phase summary on the next query.

## Pbfhogg-facing conventions

Two conventions pbfhogg developers need to know about (documented in
README and in `brokkr sidecar --help`):

- **`WAIT_*` stall spans.** Wrap blocking points in named spans whose
  name begins `WAIT_` so `--stalls` can attribute stall time by
  category. Categories are free-form - pbfhogg decides on
  `WAIT_WRITER`, `WAIT_PAYLOAD`, `WAIT_COORD`, etc. as the ALTW
  rewrites expose interesting blocking points.
- **Span naming for `--stop`.** `MARKER_END` is still the canonical
  form; the `-MARKER` shorthand is purely a CLI convenience and
  doesn't change the emission side.

## Explicit non-goals

Everything the two earlier drafts proposed beyond the four items above
is **deferred until a concrete ALTW pain point justifies it**:

- Span IDs, parent links, tags.
- Typed counter discrimination (monotonic vs gauge).
- New protocol discriminators (`+`/`-`/`@`/`~`/`#`/`!`).
- Protocol version negotiation.
- New tables (`sidecar_spans`, `sidecar_gauges`, `sidecar_run_attrs`,
  `sidecar_events`).
- Virtual spans / derived metrics / recipes / keep-revert verdicts.
- Metric metadata (units/kind/scope).
- Detail modes (`--detail worker|epoch|all`).
- Pbfhogg sidecar crate / RAII `span!()` macro.
- Migrating pbfhogg's 228 marker pairs to a new API.

Pointer-only: if after running ALTW opportunity #1 against the
enriched phase summary + `--stalls` view you find a question the
current shape genuinely can't answer, that's the trigger for
revisiting the v2 design - informed by *use*, not speculation.

## Acceptance

- `brokkr sidecar <uuid>` (default phase summary) shows
  `user_cores`, `kernel_cores`, `majflt_delta`, `minflt_delta`,
  `vol_cs_delta`, `nonvol_cs_delta`, `peak_threads` per phase in
  both JSONL and `--human` table output.
- `brokkr sidecar <uuid>` surfaces truncated spans and
  `dropped_events > 0` when present.
- `brokkr sidecar <uuid> --stalls` produces a stall-by-category
  breakdown from `WAIT_*` span durations, with clear messaging on runs
  that have no `WAIT_*` spans.
- `brokkr sidecar <uuid> --stalls --human` emits a fixed-width table.
- `brokkr <cmd> --stop FOO` resolves to `FOO_END` with a one-line
  notice when the fallback fires.
- All of the above works unchanged against existing `.brokkr/sidecar.db`
  rows from before this arc.
