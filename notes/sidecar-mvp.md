# Brokkr MVP — let's do this now

Status: **design, scoped for near-term implementation.** Drafted 2026-04-16
as the "let's do this now" complement to `notes/sidecar-protocol-v2.md`.

## Framing

`sidecar-protocol-v2.md` describes the perfect world: span IDs, structured
tags, experiment recipes, keep/revert verdicts, virtual spans, critical-path
analysis. That doc is correct and substantial — weeks of work.

This doc is the **narrowly-scoped subset we land now**: the typed FIFO
protocol change (as proposed to pbfhogg dev 1, refined per their feedback)
plus the one convention addition that was clearly worth pulling forward
from pbfhogg dev 2's wishlist — `WAIT_*` spans for stall attribution.

Everything else dev 2 proposed (virtual spans, critical path, recipes,
detail modes, structured metadata, span IDs, tags) stays in the v2 doc
until a concrete ALTW pain point justifies the work.

## Goals

Unblock pbfhogg's ALTW structural-rewrite benchmarking by answering:

1. **What held wall time open?** (stall attribution via `WAIT_*` spans)
2. **Where was CPU spent — user or kernel?** (per-span `/proc` expansion)
3. **Was the phase IO-bound, memory-pressured, or CPU-bound?** (per-span
   page faults, context switches, thread-count peak)
4. **Which counters aggregate as deltas vs min/max/avg?** (typed
   discriminator, not name-suffix guesswork)

## Non-goals

- **No span IDs.** Parallel same-named spans remain ambiguous in MVP; pbfhogg
  lives with that or uses per-instance names ad-hoc. Full fix waits for v2.
- **No structured tags.** No `worker=5 epoch=2 bucket=17` on spans; if
  pbfhogg wants that info, encode it in the name.
- **No virtual spans / derived metrics.** Brokkr compares by span name,
  period. Topology rewrites that rename/fuse phases will need manual
  translation for comparison until v2.
- **No critical-path view.** Deferred.
- **No experiment recipes / keep-revert verdicts.** Dev judges manually.
- **No detail modes (`--detail worker`).** Deferred.
- **No in-process aggregator crate.** Pbfhogg continues to roll its own
  (e.g. atomic counters flushed at stage end); brokkr doesn't provide a
  helper API.
- **No `close_with()` API.** Deferred.
- **No structured metric metadata.** Units, kind, scope as first-class
  fields is v2.

## Protocol v2 (FIFO line format)

```
+<name>                # span start
-<name>                # span end
@<name>=<value>        # monotonic counter (last-wins within span)
~<name>=<value>        # gauge (time-weighted aggregation)
#<key>=<value>         # run-level attribute (num_workers, compression, …)
!<reason>[=<value>]    # sidecar meta event (dropped_events=N, etc.)
```

Timestamp prefixes every line as today: `<us> +<name>`, `<us> @<name>=<val>`, etc.
The leading discriminator character distinguishes event kinds.

### Pairing without IDs

Brokkr pairs `+<name>` to the next matching `-<name>` (first-start / first-end
per name). For MVP, pbfhogg agrees that any `+<name>` / `-<name>` pair is
serial and non-overlapping per name — no parallel worker threads emit the
same name concurrently. Parallelism within the same name waits for v2 span IDs.

### Monotonic counter semantics — per dev 1's refinement

"Monotonic" here means **last-wins within a span**, not "delta over in-span
samples." That matches pbfhogg's dominant pattern: an `AtomicU64` accumulator
updated throughout the stage, flushed once via `emit_counter(…)` near the
`-<stage>` event. Brokkr reads the final value; it is already the
cumulative-to-end-of-stage number the emitter intended.

For counters emitted periodically (rare today — pbfhogg's code emits once-at-
end almost exclusively), last-wins is still the right answer: the latest
value is the running total as of that moment.

### Gauge semantics

Step-function between emissions. Brokkr's per-span gauge aggregation uses
time-weighted integration: `∫ value · dt / ∫ dt` over the span's time range.

Per dev 1's caveat: this math only earns its keep when pbfhogg emits gauges
**periodically** within a span (sampling some dynamic quantity — queue depth,
pending-buffer size, live worker count). For today's "one-shot high-water
snapshot at stage end" patterns (writer_reorder_high_water=523,
s4_channel_high_water=54), a gauge with one sample collapses to
min=max=avg=last, which is correct but no richer than the current counter.
Usefulness of time-weighted math unlocks as pbfhogg adopts periodic gauge
emission at points where it matters.

### Protocol version negotiation

Child process sets `BROKKR_SIDECAR_PROTOCOL=2` in its env, emits
`#sidecar_protocol=2` as its first event. Sidecar parses per-version. v1
streams continue to parse unchanged.

## `WAIT_*` stall spans (convention)

Pbfhogg wraps blocking points in named spans whose name begins `WAIT_`:

```rust
{
    let _span = sidecar::span!("WAIT_WRITER");
    writer_channel.send(payload)?;
} // drop emits -WAIT_WRITER
```

Brokkr recognises the `WAIT_*` prefix in queries — a new `--stalls` view
sums matching span durations by category (`WAIT_WRITER`, `WAIT_PAYLOAD`,
`WAIT_COORD`, `WAIT_SPILL`, `WAIT_DECODE`, …), displayed as a fraction of
the enclosing top-level span's wall time.

This is a **convention**, not a protocol feature. Brokkr doesn't validate
the naming; any span named `WAIT_FOO` is treated as a stall in category
`FOO`. The convention is documented in brokkr's README and pbfhogg adopts
it for the blocking points that matter most to the ALTW rewrites.

## Pbfhogg API

A thin sidecar crate pbfhogg depends on:

```rust
// Span RAII — Drop emits -<name>, no mismatch possible.
let _span = sidecar::span!("EXTJOIN_STAGE1");

// Typed counter / gauge emission.
sidecar::emit_counter("bytes_processed", n);    // @name=val, monotonic
sidecar::emit_gauge("pending_buffer", d);       // ~name=val, gauge

// Run-level attributes (emitted once at run start).
sidecar::run_attr("num_workers", cfg.workers);
sidecar::run_attr("compression", "zstd:3");
sidecar::run_attr("index_type", "external");
```

## Brokkr storage

New tables alongside the existing v1 tables:

```sql
CREATE TABLE sidecar_spans (
    result_uuid    TEXT NOT NULL,
    run_idx        INTEGER NOT NULL,
    span_idx       INTEGER NOT NULL,      -- order-of-opening within run
    name           TEXT NOT NULL,
    start_us       INTEGER NOT NULL,
    end_us         INTEGER,                -- NULL = truncated
    status         TEXT NOT NULL,          -- ok | truncated | orphan
    PRIMARY KEY (result_uuid, run_idx, span_idx)
);

CREATE TABLE sidecar_gauges (
    result_uuid  TEXT NOT NULL,
    run_idx      INTEGER NOT NULL,
    timestamp_us INTEGER NOT NULL,
    name         TEXT NOT NULL,
    value        REAL NOT NULL
);

CREATE TABLE sidecar_run_attrs (
    result_uuid  TEXT NOT NULL,
    run_idx      INTEGER NOT NULL,
    key          TEXT NOT NULL,
    value        TEXT NOT NULL,
    PRIMARY KEY (result_uuid, run_idx, key)
);

-- Existing sidecar_counters gains a kind column.
ALTER TABLE sidecar_counters ADD COLUMN kind TEXT NOT NULL DEFAULT 'monotonic';
```

**No raw-event retention.** `sidecar_events` (as in v2) deferred. V1 kept
marker and counter rows; v2 keeps marker/span + counter/gauge + run_attr.
Raw-events-as-truth comes later.

## Brokkr views

### `--timeline --summary` (extended)

Per-span JSONL record expands with:

```json
{
  "type": "phase",
  "name": "EXTJOIN_STAGE4",
  "duration_ms": 90938,
  "samples": 909,
  "peak_rss_kb": 3213760,
  "disk_read_kb": 64118176,
  "disk_write_kb": 29515396,
  "avg_cores": 18.2,
  "kernel_cores": 2.4,        // new: stime / wall_time
  "user_cores": 15.8,         // new: utime / wall_time
  "majflt_delta": 1247,       // new: major page faults
  "minflt_delta": 892341,     // new: minor page faults
  "vol_cs_delta": 8945,       // new: voluntary context switches
  "nonvol_cs_delta": 312,     // new: preemptions
  "peak_threads": 49,         // new: max thread count
  "counters": {               // new: monotonic counters, last-wins in span
    "bytes_processed": 4523789012,
    "s4_coord_payload_pread_ms": 1824
  },
  "gauges": {                 // new: time-weighted aggregation
    "pending_buffer": {"min": 0, "max": 41, "avg": 12.4, "last": 0}
  }
}
```

### `--stalls` (new)

```
brokkr results <uuid> --stalls
```

JSONL: one record per stall category, summing matching `WAIT_*` span
durations. `--human` gives a table.

### `--compare-timeline` (extended)

Adds the new per-span fields (kernel/user cores, fault deltas, etc.) for
both runs with per-field deltas. Compared phases still align by span name
only — topology-aware comparison lives in v2.

## `--stop` transition — per dev 1

Today: `--stop FOO_END` fires on a marker named literally `FOO_END`.
After MVP:

1. **This release:** `--stop FOO_END` accepted, prints a one-line
   deprecation warning, internally matches `-<span FOO>`.
2. Preferred new form: `--stop FOO` or `--stop -FOO` (either matches span
   close).
3. **Next release:** drop the `_END` alias, keep warning only if users ask.

## Truncation and drop accounting

- **Truncated spans.** At child exit, any `+<name>` without matching
  `-<name>` becomes `{status: truncated, end_us: child_exit_us}` in
  `sidecar_spans`. Surfaces in `brokkr results <uuid>` detail view.
- **Dropped events.** Sidecar reports FIFO write failures via `!dropped_events=N`
  events; brokkr flags any run with `dropped_events > 0` in the detail view.

## Migration from v1

When brokkr's sidecar reader encounters v1 format (no `+`/`-`/`~`/`#`/`!`
discriminators), it continues to parse as before. Existing v1 sidecar.db
rows stay readable through the v1 code paths, which remain in place.

New v2 runs populate the new tables. No data migration of historical rows —
v1 views for v1 runs, v2 views for v2 runs. If a user asks `--stalls` on a
v1 run, brokkr says "this run predates v2; no stall data."

Pbfhogg's ~228 `_START`/`_END` marker pairs migrate to `span!()` calls —
mechanical but thorough; closer to a day's careful work per dev 1. Goes
hand-in-hand with adopting the counter kind split (monotonic default for
existing counters; handful relabelled as gauges if they're genuinely
point-in-time readings).

## Sequence

Per dev 1's hard requirement: don't overlap protocol migration with an
ALTW keep/revert benchmark decision.

1. **Brokkr:** land v2 FIFO parser + new storage tables + protocol version
   negotiation. v1 streams continue to parse.
2. **Brokkr:** extend `--timeline --summary` JSONL with the new `/proc`
   per-span metrics (user/kernel cores, fault deltas, ctxt switches,
   threads). Ships independent of the protocol change.
3. **Brokkr:** implement `--stalls` view.
4. **Pbfhogg:** migrate `_START`/`_END` marker pairs to `span!()`; mark
   counters monotonic by default, relabel gauges where appropriate;
   adopt `WAIT_*` convention at stage boundaries. Add `run_attr` for
   `num_workers`, `compression`, `index_type`, `keep_untagged_nodes`,
   `num_epochs`.
5. **Refresh Europe / planet baseline on v2 format.**
6. **Start ALTW opportunity #1 benchmarks** against the v2 baseline.

## Acceptance criteria

MVP is done when, on a real ALTW benchmark:

- `brokkr results <uuid> --timeline --summary` shows per-phase user/kernel
  CPU split, page faults, context switches, and peak thread count.
- Per-span counter values appear in the JSONL record with correct
  last-wins-in-span semantics.
- `brokkr results <uuid> --stalls` sums `WAIT_*` span durations by
  category.
- A run killed by `--stop` (or OOM or panic) produces spans with
  `status=truncated` and surfaces `dropped_events=N` when applicable.
- `--stop FOO_END` still works on v2 runs with a deprecation warning.
- v1 sidecar.db rows continue to render in their existing views.

## Deferred to protocol v2

Pointer only; see `sidecar-protocol-v2.md` for detail.

- Span IDs + tags (solves parallel-worker collision properly, enables
  topology-invariant comparison).
- Raw-event retention as primary storage.
- Virtual spans and derived metrics.
- Critical-path and stall-attribution analytical views beyond the flat
  `--stalls` sum.
- Experiment recipes with keep/revert reporting.
- In-process aggregators and `close_with()` API.
- Units / kind / scope as structured metadata.
- Detail modes (`--detail worker|epoch|all`).
