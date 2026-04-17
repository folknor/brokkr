# Brokkr v2 — structured spans, keep/revert decisions

Status: **design, not yet implemented.** Drafted 2026-04-16 after two rounds of
feedback from pbfhogg developers reviewing successive drafts.

## Purpose

> brokkr tells us, quickly and truthfully, whether a structural rewrite is worth keeping.

Not "more metrics." Not "prettier graphs." The question brokkr has to answer is:
*given a topology change, do we keep it or revert?* Every other capability in this
doc is a means to that end.

## Motivation

Brokkr's sidecar gives us two streams today: kernel signal (100ms `/proc` sampling)
and application signal (FIFO markers + counters). The kernel side works. The
application side breaks for the ALTW-era work ahead:

- **Markers are spans in a bookmark's clothes.** All 228 distinct marker names in
  real sidecar.db end in `_START` or `_END` — pbfhogg encodes span semantics in a
  naming convention the protocol doesn't know about. First-start-to-first-end
  pairing silently drops 21 of 22 parallel `WORKER_START` events when stage 1
  pass B runs with 22 threads.
- **Counters conflate kinds.** `s1a_pread_ms=7293` is a cumulative accumulator
  emitted once at stage end. `extract_ways_written=167` is a one-shot total.
  `queue_depth=42` (wanted, not present today) would be a gauge. Brokkr can't
  aggregate them per-phase without knowing which math applies.
- **Parallel same-named structures can't be represented.** Stage 1 pass B = 22
  workers. Stage 2 = 6. Stage 4 = 22 decode threads. Any per-worker span
  instrumentation silently collides.
- **Incomplete runs aren't visible.** FIFO writes can drop under O_NONBLOCK; the
  child can be killed by `--stop`, OOM, or panic. Brokkr has no way to surface
  "this run's semantic trace is partial."

The ALTW structural rewrites described in pbfhogg's `notes/altw-structural-reports.md`
push explicitly toward overlapping work, stream fusion, deleted/fused stages, and
repeated epoch/bucket phases. A protocol that can't represent parallelism, can't
distinguish counter kinds, and relies on name-suffix conventions is the wrong
foundation for evaluating those rewrites — and a flat phase-by-phase comparison
view is the wrong lens for judging them.

## Goals

1. **Unambiguous span identity.** Parallel same-named spans don't collide.
2. **Structural parent/child relationships.** Explicit, not inferred from time overlap.
3. **Typed counters and gauges.** Per-phase math is right by construction.
4. **Tags on spans.** `worker=5 epoch=2 bucket=17` as structured attributes,
   not name-encoded.
5. **Raw event retention.** The FIFO trace is the source of truth; derived tables
   rebuildable.
6. **Visible incompleteness.** Dropped events and truncated spans are first-class.
7. **Topology-aware comparison.** Not just "phase A vs phase A" — support virtual
   spans and derived metrics so fused/overlapped/deleted stages can still be
   compared against a baseline.
8. **Decision-driving output by default.** Compact summary that answers "keep or
   revert"; deep per-worker/epoch trace opt-in.
9. **Experiments as first-class objects.** A benchmark recipe — correctness gate +
   primary metric + secondary gates + thresholds — is something brokkr runs and
   reports on, not a manual checklist the human keeps in their head.

## Non-goals

- Real-time observability (production tracing, distributed spans). Brokkr is
  dev-loop tooling; one binary, one run.
- Nanosecond precision. 100ms `/proc` sampling bounds useful resolution.
- Cross-project standards. The protocol serves brokkr ↔ pbfhogg and can evolve
  freely.
- Hierarchical sampling decisions. Spans are all-or-nothing per run; detail
  levels via opt-in flags.

## Protocol v2 (FIFO line format)

```
+<span_id> <name> [parent=<id>] [k=v ...]    # span start
-<span_id> [k=v ...]                         # span end  (may attach close-time metrics)
@<name>=<value>                              # monotonic counter, last-wins within span
~<name>=<value>                              # gauge, step-function (time-weighted aggregation)
#<key>=<value>                               # run-level attribute
!<reason>[=<value>]                          # sidecar meta event (dropped_events, truncated, etc.)
```

### Event grammar

- `span_id` is an atomic `u64` that the pbfhogg process assigns. Unique within a
  run. Sidecar never generates IDs.
- Timestamps prefix every line: `<us> +<id> <name> …`. Discriminator character
  determines kind.
- Tag keys match `[a-zA-Z_][a-zA-Z0-9_-]*`. Values are strings (unquoted, no
  whitespace; use `_` as separator or escape).
- `parent=<id>` is optional; absent means top-level. Orphaned children are
  recorded but flagged.
- Close-time tags merge with start-time tags (close wins on collision).
- Protocol version negotiated via `BROKKR_SIDECAR_PROTOCOL=2` env var and an
  initial `#sidecar_protocol=2` line from the child.

## Pbfhogg API

A thin sidecar crate pbfhogg depends on:

```rust
// Span RAII — cannot mismatch.
let _span = sidecar::span!("EXTJOIN_STAGE1");
let _span = sidecar::span!(
    "EPOCH_EMIT",
    parent = parent_id,
    epoch = 2,
    worker = 5,
);

// Counters.
sidecar::emit_counter("bytes_processed", n);    // @, last-wins-in-span
sidecar::emit_gauge("pending_buffer", d);       // ~, time-weighted

// In-process aggregators — no FIFO traffic per observation.
let watermark = span.high_water("peak_queue_depth");
watermark.observe(d);
// Flushes one event on span close.

// Close-time metric attachment.
span.close_with([
    ("max_workers", 22),
    ("total_bytes", 4_500_000_000),
    ("epoch_count", 7),
]);

// Run-level attributes (emitted once at run start).
sidecar::run_attr("num_workers", cfg.workers);
sidecar::run_attr("compression", "zstd:3");
sidecar::run_attr("index_type", "external");
```

Span IDs are transparent — allocated by `span!()` and hidden inside the `Span`
type. Dropped `Span` emits `-<id>` on Drop, so spans close even in panic-unwind
paths.

## Brokkr storage

```sql
-- Raw truth: one row per FIFO line, emission order.
CREATE TABLE sidecar_events (
    result_uuid  TEXT NOT NULL,
    run_idx      INTEGER NOT NULL DEFAULT 0,
    event_idx    INTEGER NOT NULL,              -- monotonic within run
    timestamp_us INTEGER NOT NULL,
    kind         TEXT NOT NULL,                 -- span_start|span_end|counter|gauge|run_attr|meta
    name         TEXT,
    value        TEXT,                          -- stringified; typed on read via metadata
    span_id      INTEGER,
    parent_id    INTEGER,
    tags_json    TEXT,                          -- {"worker":5,"epoch":2}
    PRIMARY KEY (result_uuid, run_idx, event_idx)
);

-- Derived: one row per closed span.
CREATE TABLE sidecar_spans (
    result_uuid     TEXT NOT NULL,
    run_idx         INTEGER NOT NULL,
    span_id         INTEGER NOT NULL,
    name            TEXT NOT NULL,
    parent_id       INTEGER,
    start_us        INTEGER NOT NULL,
    end_us          INTEGER,                    -- null for truncated
    status          TEXT NOT NULL,              -- ok|truncated|orphan
    start_tags_json TEXT,
    close_tags_json TEXT,                       -- also carries close_with metrics
    PRIMARY KEY (result_uuid, run_idx, span_id)
);

CREATE TABLE sidecar_counters (
    result_uuid  TEXT NOT NULL,
    run_idx      INTEGER NOT NULL,
    timestamp_us INTEGER NOT NULL,
    name         TEXT NOT NULL,
    value        REAL NOT NULL,
    kind         TEXT NOT NULL                  -- monotonic|gauge
);

CREATE TABLE sidecar_run_attrs (
    result_uuid  TEXT NOT NULL,
    run_idx      INTEGER NOT NULL,
    key          TEXT NOT NULL,
    value        TEXT NOT NULL,
    unit         TEXT,                          -- ns|bytes|count|bool|ratio|items|ms|kb|...
    scope        TEXT NOT NULL,                 -- run|span
    PRIMARY KEY (result_uuid, run_idx, key)
);
```

v1 migration:

- `sidecar_markers` → `sidecar_events` with kind `span_start`/`span_end` inferred
  from `_START`/`_END` suffix; span IDs fabricated by pair-matching.
- `sidecar_counters` → `sidecar_counters` with kind `monotonic`.
- Unpaired markers → `span_start` with status `truncated`.

Historical rows display correctly but without tags, parent links, or close-time
metrics.

## Virtual spans and derived metrics

Topology changes (fused stages, deleted stages, overlapped work) break "phase A
vs phase A" comparisons. Virtual spans are user-defined composites that compare
*semantic work*, not *structural phases*.

A virtual span is defined by a recipe stored in the project (e.g.
`.brokkr/virtual_spans.toml`):

```toml
[virtual_spans.downstream_slice]
summation = ["EXTJOIN_STAGE2", "EXTJOIN_STAGE3", "COORD_PAYLOADS_FINALIZE", "EXTJOIN_STAGE4"]
metric = "duration_ms"

[virtual_spans.payload_path]
summation = ["EXTJOIN_STAGE3_EMIT", "EXTJOIN_STAGE4_WAIT_FOR_PAYLOAD"]
metric = "duration_ms"

[virtual_spans.writer_bound_fraction]
# Fraction of stage 4 wall time spent waiting for writer flushes.
formula = "sum(EXTJOIN_STAGE4_WAIT_WRITER.duration_ms) / EXTJOIN_STAGE4.duration_ms"
unit = "ratio"
```

`brokkr results --virtual-span downstream_slice <uuid>` returns one value.
`brokkr results --compare <uuid_a> <uuid_b> --virtual-span downstream_slice`
compares across commits.

This is how "the old downstream slice" stays comparable to "the new overlapped
critical path" after a rewrite deletes `EXTJOIN_STAGE3` and fuses its work into
stage 2.

## Stall attribution

For ALTW, "where was CPU spent?" is often less important than "what held wall
time open?" Brokkr needs first-class support for stall attribution:

- **Critical path.** Longest chain through the span tree (by wall-clock). If
  stage 4 overlaps stage 3, the critical path might skip stage 3 entirely.
- **Wait spans.** Pbfhogg instruments explicit `WAIT_WRITER` / `WAIT_PAYLOAD` /
  `WAIT_COORD` spans around blocking points. Brokkr's `--stall` view shows
  total wait time by cause, plus the span hierarchy where the waits live.
- **Queue/backpressure metrics.** High-water + dwell time (how long was the
  queue >N full?). Requires gauge emission from the producer side, plus
  in-process dwell-time accumulators on the consumer side.
- **Spill vs in-memory split.** Monotonic counters (`spill_bytes`,
  `in_memory_bytes`) on the emission side. Brokkr per-span view shows both
  totals plus the ratio.
- **Scratch I/O by stage/span.** Per-span deltas of `/proc/pid/io.read_bytes` /
  `write_bytes` already available from samples; expose as `scratch_read_kb` /
  `scratch_write_kb` in the span summary.

These aren't just queries — they require coordinated instrumentation in pbfhogg
and specific span/counter names brokkr knows to look for.

## Experiment recipes — keep/revert workflow

Pbfhogg's ALTW plan has keep/revert decisions with explicit gates. Brokkr
should run those as recipes, not leave them as manual invocation:

```toml
# .brokkr/experiments/altw-opp1.toml

name = "altw-opp1-epoch-spill"
baseline = "ada4ae72"

[correctness]
command = "brokkr verify add-locations-to-ways --dataset denmark --mode all"

[primary]
metric = "duration_ms"
span = "EXTJOIN_STAGE3"     # or a virtual_span reference
command = "brokkr add-locations-to-ways --dataset europe --bench 3"
threshold_pct = -5          # require ≥5% improvement
direction = "decrease"

[[secondary]]
metric = "peak_rss_kb"
threshold_pct = 10          # reject if RSS grows >10%
direction = "decrease-or-flat"

[[secondary]]
metric = "scratch_bytes_written"
threshold_pct = -20
direction = "decrease"

[[secondary]]
metric = "virtual_spans.writer_bound_fraction"
threshold = 0.3             # writer-bound fraction must drop below 30%
direction = "decrease"

[confirm]
# Optional — only runs if primary passes.
command = "brokkr add-locations-to-ways --dataset planet --bench 1"
```

`brokkr experiment run altw-opp1`:

1. Runs the correctness gate. Aborts with a clear message if Denmark fails.
2. Runs the primary bench.
3. Pulls the baseline UUID's matching span from results.db.
4. Computes primary + secondary deltas with thresholds.
5. Runs planet confirmation if primary passed.
6. Emits a **keep/revert report**: one explicit verdict, plus the per-gate
   reasoning and per-metric numbers.

Report shape (JSONL by default, `--human` for a table):

```json
{"type":"experiment","name":"altw-opp1-epoch-spill","verdict":"keep","primary":{"metric":"duration_ms","baseline":329400,"candidate":312100,"delta_pct":-5.26,"threshold":-5,"passed":true},"secondary":[{"metric":"peak_rss_kb","delta_pct":3.2,"threshold":10,"passed":true}, ...],"correctness":"passed","confirm":{"baseline_ms":1890000,"candidate_ms":1820000,"delta_pct":-3.7}}
```

This is the difference between "brokkr has lots of numbers" and "brokkr says
*keep this change*."

## Units and metric metadata

Every metric knows:

- **Unit**: `ns`, `us`, `ms`, `s`, `bytes`, `kb`, `mb`, `count`, `ratio`, `bool`,
  `items`.
- **Kind**: `monotonic`, `gauge`, `attribute`.
- **Scope**: `run` or `span`.

Registered once per run at the top, not per-emission:

```rust
sidecar::register_metric("bytes_processed", Unit::Bytes, Kind::Monotonic, Scope::Span);
sidecar::register_metric("num_workers", Unit::Count, Kind::Attribute, Scope::Run);
```

Stored in `sidecar_run_attrs` as `#meta.bytes_processed={"unit":"bytes","kind":"monotonic","scope":"span"}`.
Brokkr uses the metadata to:

- Pick the right aggregation rule (monotonic → last-wins, gauge → time-weighted avg).
- Auto-scale displayed values (`48044512 kB` → `48.0 GB`).
- Compute ratios sanely (`duration_ms / bytes_processed` is "ms per byte").
- Flag compare mismatches (can't subtract a bool from a bool in a compare delta).

## Decision-driving output vs debug output

Two explicit modes:

- **Default** (decision-driving): virtual spans, primary and secondary metrics,
  keep/revert verdicts, stall summary. Compact. Answers "did this change help?"
- **`--detail <level>`** (debug): `--detail worker` shows per-worker spans,
  `--detail epoch` shows per-epoch, `--detail all` shows every span. JSONL size
  can grow 100x; pairs with `jq` / a real tool, not human-scan.

Same data, different filters. The default stays scannable even as ALTW adds
streams of per-worker instrumentation.

## `--stop` transition

Today: `--stop FOO_END` fires on a marker named `FOO_END`. In v2, the equivalent
is "on close of span named FOO". Transition:

1. Release N: `--stop FOO_END` accepted, prints deprecation warning, matches
   `-<span name FOO>`.
2. `--stop FOO` and `--stop -FOO` match span close; `--stop +FOO` matches span
   start; `--stop FOO#3` matches the 3rd occurrence (for repeated epoch spans).
3. Release N+1: drop the `_END` alias.

## Truncation and drop accounting

- **Truncated spans.** At child exit, any `+<span_id>` without matching
  `-<span_id>` becomes `{status: truncated, end_us: child_exit_us}` in
  `sidecar_spans`. `brokkr results <uuid>` shows truncated-span count;
  JSONL emits `"status": "truncated"`.
- **Dropped events.** Sidecar tracks FIFO write failures and emits
  `!dropped_events=<n>` into `sidecar_events`. `brokkr results` flags any run
  with `dropped_events > 0`.
- **Orphaned events.** `-<id>` without a matching `+<id>`, or a `parent=<id>`
  pointing to nothing, marks the span `orphan`.

## Prioritized work order (brokkr dev)

Per pbfhogg dev 2's recommendation — foundation first, then the decision-driving
layer, then polish:

1. **Typed spans.** IDs, tags, parent links, truncation handling, raw-event
   retention, proper gauge math, counter kinds. The foundation. Everything
   below is blocked by it.
2. **Derived metrics / virtual spans / compare-by-tags.** Topology-invariant
   comparisons. Load-bearing for judging ALTW rewrites that delete or fuse
   stages.
3. **Critical-path and stall attribution.** Answers "what held wall time open?"
   for overlapped work. Requires matched instrumentation in pbfhogg.
4. **Experiment recipes with keep/revert reporting.** Turns benchmarking from a
   manual ritual into an automated decision. Reuses #1–#3 as inputs.
5. **Metric metadata and units.** Makes summaries and compares error-proof.
   Could be done earlier but depends on #1's schema.
6. **Optional deep-trace detail modes.** Worker/bucket/epoch spans gated by
   `--detail`. Low priority — opt-in, hurts no one if late.

## ALTW landing sequence

Separate from the brokkr-internal priority above, this is the *when-do-we-do-what*
for pbfhogg's benchmark work:

1. Land v2 protocol in brokkr (#1 and as much of #2 as needed for keep/revert).
2. Migrate pbfhogg's ~228 marker pairs to `span!()`. Add `run_attr` calls.
   Mark counters monotonic/gauge.
3. **Refresh Europe / planet baseline on v2 format.** Dedicated step. Clean
   numbers, current format.
4. Implement brokkr recipe support (#4 in brokkr priority). Write ALTW
   opportunity recipes.
5. **Then** run ALTW opportunity #1 benchmarks with clean before/after.

Do NOT overlap protocol migration with opportunity #1's keep/revert decision.

## Open questions

1. **Detail-level mechanism.** Env var (`BROKKR_SPAN_DETAIL=worker`) or Cargo
   feature? Env var is cheaper but leaks into production-like runs; feature is
   stricter but requires rebuilds. Leaning env var with a per-recipe override.
2. **Span ID collision under multi-process.** Not a concern today (one
   brokkr run = one child). If we ever bench a client + server together,
   atomic counters per process would collide. Defer: add a process-id prefix
   if we hit that case.
3. **FIFO buffer size / backpressure.** Today O_NONBLOCK silently drops. Could
   switch to a larger pipe + flush-periodically, or make drops visible via the
   `!dropped_events` mechanism defined above. Needs measurement before deciding.
4. **Virtual span composition operators.** The `summation = [...]` and
   `formula = "..."` forms above cover most cases. Do we also need
   `max_of`, `time_aligned_union`, `critical_chain`? Start minimal; grow when
   a concrete ALTW recipe needs it.
5. **Recipe storage location.** `.brokkr/experiments/*.toml` per-project, or
   inline in `brokkr.toml`, or in pbfhogg's `notes/`? Leaning per-file in
   `.brokkr/experiments/` — reproducibility matters, and git-tracking per-file
   keeps diffs readable.
6. **Keep/revert threshold semantics.** Is `threshold_pct = -5` "improvement
   of at least 5%" or "delta no worse than -5%"? Needs spelling out in the
   recipe schema. Leaning: `direction = "decrease"` + `threshold_pct = 5`
   reads unambiguously.

## Acceptance criteria

Protocol v2 lands successfully when:

- Brokkr parses v2 FIFO events into `sidecar_events` without loss.
- `sidecar_spans` correctly represents parallel same-named spans (22 concurrent
  workers → 22 rows, not 1).
- `--timeline --summary` JSONL output carries per-span counters and gauges with
  correct aggregation math (monotonic last-wins, gauge time-weighted).
- `--compare-timeline` aligns by name + tags; 22-worker vs 6-worker runs show
  matched + unmatched rows, no silent collapse.
- `--virtual-span <name>` resolves composite metrics defined in
  `.brokkr/virtual_spans.toml`.
- A truncated run surfaces `status=truncated` spans and `dropped_events > 0` in
  the detail view.
- `brokkr experiment run <recipe>` produces a JSONL verdict (`keep`/`revert`)
  with per-gate reasoning.
- v1 sidecar.db rows render in `--human` views after migration.

## What this doesn't do

- Doesn't change `/proc` sampling cadence or fields.
- Doesn't touch `results.db` schema.
- Doesn't change `brokkr download`, `brokkr check`, `brokkr env`, or any
  non-bench workflow.
- Doesn't impose any protocol on elivagar/nidhogg/sluggrs. They can adopt v2
  when they want; v1 continues to work.
