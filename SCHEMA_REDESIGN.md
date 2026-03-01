# brokkr Results Database Schema Redesign: v2 -> v3

## Overview

The current schema is a single `runs` table with 20 columns. Everything from
simple timing benchmarks to hotpath function-level profiling is crammed into
the same rows, with `extra` (JSON TEXT) doing all the heavy lifting for
structurally different data. This proposal normalizes the schema into a
parent-child table hierarchy with a common envelope, structured child tables
for known data shapes, and a key-value overflow table for the long tail.

## Current Problems (recap)

1. No first-class peak RSS (VmHWM). Memory metrics exist only in hotpath's
   `extra` JSON as point-in-time RSS.
2. One table for everything. Timing benchmarks, allocation profiles, hotpath
   function tables, merge stats, CLI command timings, elivagar phase metrics
   all share the same schema.
3. `extra` is a grab bag. Sometimes `{min_ms, p50_ms, ...}`, sometimes nested
   function profiling arrays, sometimes subprocess key=value metrics.
4. No structured storage for per-phase metrics. Phase-level RSS sampling,
   blob-size distributions, and per-phase timing cannot be modeled.
5. No way to store elivagar/nidhogg-specific metrics without polluting the core.

## Design Principles

- **Envelope + children**: every run gets exactly one row in `runs` (the
  envelope). Specialized data lives in child tables keyed by `run_id`.
- **Structured where shape is known**: distribution stats, hotpath functions,
  hotpath threads, merge stats all get dedicated child tables.
- **Key-value overflow**: anything that does not fit a structured table goes
  into `run_kv` as typed key-value pairs, replacing the JSON grab bag.
- **No JSON columns**: all data is stored in native SQLite types. The `extra`
  and `metadata` columns are replaced by child tables.
- **Backward compatible migration**: all existing v2 data is preserved and
  migrated to the new structure.

---

## Schema Diagram

```
runs (envelope)                      run_distribution
+------------------+                 +------------------+
| id (PK)          |<------+------->| run_id (FK)      |
| uuid             |       |        | samples          |
| timestamp        |       |        | min_ms           |
| hostname         |       |        | p50_ms           |
| commit           |       |        | p95_ms           |
| subject          |       |        | max_ms           |
| command          |       |        +------------------+
| variant          |       |
| input_file       |       |        run_kv
| input_mb         |       |        +------------------+
| elapsed_ms       |       +------->| run_id (FK)      |
| peak_rss_mb      |       |        | key              |
| cargo_features   |       |        | value_int        |
| cargo_profile    |       |        | value_real       |
| kernel           |       |        | value_text       |
| cpu_governor     |       |        +------------------+
| avail_memory_mb  |       |
| storage_notes    |       |        hotpath_functions
| cli_args         |       |        +------------------+
| project          |       +------->| run_id (FK)      |
+------------------+       |        | section          |
                           |        | description      |
                           |        | ordinal          |
                           |        | name             |
                           |        | calls            |
                           |        | avg              |
                           |        | total            |
                           |        | percent_total    |
                           |        | p50              |
                           |        | p95              |
                           |        | p99              |
                           |        +------------------+
                           |
                           |        hotpath_threads
                           |        +------------------+
                           +------->| run_id (FK)      |
                                    | name             |
                                    | status           |
                                    | cpu_percent      |
                                    | cpu_percent_max  |
                                    | cpu_user         |
                                    | cpu_sys          |
                                    | cpu_total        |
                                    | alloc_bytes      |
                                    | dealloc_bytes    |
                                    | mem_diff         |
                                    +------------------+
```

---

## Table-by-Table Description

### 1. `runs` (envelope)

The common envelope for every recorded benchmark, hotpath, or profile run.
One row per measurement. This is the primary table that all queries start from.

```sql
CREATE TABLE IF NOT EXISTS runs (
    id              INTEGER PRIMARY KEY,
    uuid            TEXT NOT NULL,
    timestamp       TEXT NOT NULL,
    hostname        TEXT NOT NULL,
    [commit]        TEXT NOT NULL,
    subject         TEXT NOT NULL,
    command         TEXT NOT NULL,
    variant         TEXT,
    input_file      TEXT,
    input_mb        REAL,
    elapsed_ms      INTEGER NOT NULL,
    peak_rss_mb     REAL,
    cargo_features  TEXT,
    cargo_profile   TEXT DEFAULT 'release',
    kernel          TEXT,
    cpu_governor    TEXT,
    avail_memory_mb INTEGER,
    storage_notes   TEXT,
    cli_args        TEXT,
    project         TEXT NOT NULL DEFAULT 'pbfhogg'
);
```

**Changes from v2:**

| Column | Change | Rationale |
|--------|--------|-----------|
| `uuid` | `NOT NULL` (was nullable) | All rows have UUIDs since v1 migration backfilled them. |
| `peak_rss_mb` | **NEW** nullable REAL | First-class VmHWM. Read from `/proc/self/status` or `/proc/<pid>/status` after subprocess exit. Nullable because not all benchmark modes can capture it. |
| `project` | **NEW** TEXT NOT NULL DEFAULT 'pbfhogg' | Disambiguates runs from pbfhogg, elivagar, nidhogg in the same DB. Enables per-project filtering without separate databases. |
| `extra` | **REMOVED** | Replaced by `run_distribution`, `hotpath_functions`, `hotpath_threads`, and `run_kv`. |
| `metadata` | **REMOVED** | Migrated to `run_kv` rows. The metadata JSON was always a flat object of benchmark parameters (compression, io_mode, strategy, bbox, etc.) which maps naturally to key-value pairs. |

**Rationale:** The envelope stays deliberately flat and wide. Every query
starts here, and SQLite is fastest when the primary filter columns are in the
same table. The 19 columns are all scalar and small. No JSON, no blobs.

### 2. `run_distribution` (per-run sample stats)

Stores the min/p50/p95/max/sample-count computed by `harness.run_distribution()`.
At most one row per run.

```sql
CREATE TABLE IF NOT EXISTS run_distribution (
    run_id      INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    samples     INTEGER NOT NULL,
    min_ms      INTEGER NOT NULL,
    p50_ms      INTEGER NOT NULL,
    p95_ms      INTEGER NOT NULL,
    max_ms      INTEGER NOT NULL,
    PRIMARY KEY (run_id)
);
```

**Rationale:** Currently these five fields are stuffed into `extra` as JSON.
Making them structural enables direct SQL queries like "show runs where p95
exceeds 2x p50" or "find runs with high variance" without JSON extraction.
The PRIMARY KEY on `run_id` ensures at most one distribution row per run and
makes the join free (it's the clustered index).

**Who populates:** `harness.run_distribution()` (used by `bench_api` for
curl timing distributions, and any future distribution-mode benchmarks).

### 3. `run_kv` (key-value overflow)

Replaces both `extra` (for subprocess kv pairs) and `metadata` (for benchmark
parameters). Each key has three typed value columns; exactly one is non-NULL.

```sql
CREATE TABLE IF NOT EXISTS run_kv (
    run_id      INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    key         TEXT NOT NULL,
    value_int   INTEGER,
    value_real  REAL,
    value_text  TEXT,
    PRIMARY KEY (run_id, key)
);
```

**Rationale:** The current `extra` JSON is parsed at read time by every
consumer. Structural kv pairs allow direct SQL filtering (e.g., "show merge
runs where passthrough_blobs > 1000") and avoid serde_json entirely on the
read path. The three-column typed pattern is standard for SQLite kv stores
and avoids type coercion bugs.

**Who populates:**

- `run_external_with_kv`: all non-elapsed_ms key=value pairs from subprocess
  stderr go here (currently: `passthrough_blobs`, `rewrite_blobs`,
  `passthrough_bytes`, `rewrite_bytes`, `output_bytes`, `nodes`, `ways`,
  `relations`, etc.)
- Benchmark metadata (previously in `metadata` JSON): `compression`,
  `io_mode`, `writer_mode`, `strategy`, `bbox`, `mode`, `heap_mb`,
  `alloc`, `test`, `tiles`, `internal_runs`, `nodes_millions`, `port`,
  `query`, `ocean`, `skip_to`, `compression_level`, `tool`.
- Any future subprocess metrics that do not justify a dedicated column.

**Convention:** Keys from subprocess stderr (runtime metrics) use snake_case
without prefix. Metadata keys (benchmark parameters) use `meta.` prefix:
`meta.compression`, `meta.io_mode`, `meta.alloc`, etc. This makes it trivial
to distinguish "what was measured" from "how it was configured".

### 4. `hotpath_functions` (function-level profiling)

Stores the per-function rows from `functions_timing` and `functions_alloc`
sections of the hotpath JSON report. Multiple rows per run.

```sql
CREATE TABLE IF NOT EXISTS hotpath_functions (
    id              INTEGER PRIMARY KEY,
    run_id          INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    section         TEXT NOT NULL,       -- 'timing' or 'alloc'
    description     TEXT,                -- e.g. 'wall clock', 'allocation size'
    ordinal         INTEGER NOT NULL,    -- position in the original report
    name            TEXT NOT NULL,       -- function name
    calls           INTEGER,
    avg             TEXT,                -- formatted string e.g. '205 ns'
    total           TEXT,                -- formatted string e.g. '8.81 s'
    percent_total   TEXT,                -- e.g. '42.3%'
    p50             TEXT,
    p95             TEXT,
    p99             TEXT
);
```

**Rationale:** Hotpath function data is the largest and most structurally
complex payload currently stuffed into `extra`. A typical hotpath run has
10-30 function rows across timing + alloc sections. Storing them relationally
enables:

- Direct SQL queries: "which functions got slower between commit A and B?"
- Proper diff generation without parsing JSON at display time.
- Efficient storage: each field is a column, not a JSON key.

**Note on typed values:** The `avg`, `total`, `p50`, `p95`, `p99` columns
store the **formatted strings** from the hotpath crate (e.g. "205 ns",
"8.81 s", "42.3 MB"). This matches the current `hotpath_fmt.rs` code which
operates entirely on formatted strings and uses `parse_metric()` for
comparison. Storing raw numeric values would require the hotpath crate to
emit them separately, which is a future enhancement. The current design
preserves exact roundtrip fidelity with the existing display code.

### 5. `hotpath_threads` (thread-level profiling)

Stores the per-thread rows from the `threads` section of the hotpath JSON.

```sql
CREATE TABLE IF NOT EXISTS hotpath_threads (
    id              INTEGER PRIMARY KEY,
    run_id          INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    status          TEXT,
    cpu_percent     TEXT,
    cpu_percent_max TEXT,
    cpu_user        TEXT,
    cpu_sys         TEXT,
    cpu_total       TEXT,
    alloc_bytes     TEXT,
    dealloc_bytes   TEXT,
    mem_diff        TEXT
);
```

**Rationale:** Thread data has a fixed schema and is always present in
hotpath-alloc runs. Separate table keeps it out of the function table
(different shape) and enables thread-level queries.

**Global thread stats** (the `rss_bytes`, `total_alloc_bytes`,
`total_dealloc_bytes`, `alloc_dealloc_diff` from the threads section header)
are stored in `run_kv` with keys `threads.rss_bytes`,
`threads.total_alloc_bytes`, etc.

---

## Index Strategy

```sql
-- Primary lookup patterns
CREATE INDEX IF NOT EXISTS idx_runs_uuid ON runs(uuid);
CREATE INDEX IF NOT EXISTS idx_runs_commit ON runs([commit]);
CREATE INDEX IF NOT EXISTS idx_runs_command ON runs(command);
CREATE INDEX IF NOT EXISTS idx_runs_timestamp ON runs(timestamp);
CREATE INDEX IF NOT EXISTS idx_runs_project ON runs(project);

-- Child table join indexes (run_id is already PK/covered for distribution and kv)
CREATE INDEX IF NOT EXISTS idx_hotpath_functions_run_id ON hotpath_functions(run_id);
CREATE INDEX IF NOT EXISTS idx_hotpath_threads_run_id ON hotpath_threads(run_id);

-- Key lookup for run_kv (for filtering runs by metadata values)
CREATE INDEX IF NOT EXISTS idx_run_kv_key ON run_kv(key, run_id);
```

**Rationale:**

- `idx_runs_uuid`: UUID prefix lookup (`LIKE ?||'%'`) uses this index.
- `idx_runs_commit`: commit prefix filtering is the most common query.
- `idx_runs_command`, `idx_runs_timestamp`: existing indexes, still needed.
- `idx_runs_project`: enables per-project filtering when the DB is shared.
- `idx_hotpath_functions_run_id`, `idx_hotpath_threads_run_id`: child table
  joins. Without these, every `format_compare` call would table-scan.
- `idx_run_kv_key`: enables queries like "find runs where
  meta.compression = 'zlib'" without scanning all kv rows.

`run_distribution` uses `run_id` as its PRIMARY KEY, so no separate index
is needed.

---

## Migration Plan: v2 -> v3

The migration runs inside a single transaction. It is split into phases for
clarity but executes atomically.

### Phase 1: Schema additions

```sql
-- Add new columns to runs
ALTER TABLE runs ADD COLUMN peak_rss_mb REAL;
ALTER TABLE runs ADD COLUMN project TEXT NOT NULL DEFAULT 'pbfhogg';

-- Make uuid NOT NULL in spirit (cannot ALTER NOT NULL in SQLite, but
-- all existing rows have uuids from the v1 migration)

-- Create child tables
CREATE TABLE IF NOT EXISTS run_distribution (
    run_id      INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    samples     INTEGER NOT NULL,
    min_ms      INTEGER NOT NULL,
    p50_ms      INTEGER NOT NULL,
    p95_ms      INTEGER NOT NULL,
    max_ms      INTEGER NOT NULL,
    PRIMARY KEY (run_id)
);

CREATE TABLE IF NOT EXISTS run_kv (
    run_id      INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    key         TEXT NOT NULL,
    value_int   INTEGER,
    value_real  REAL,
    value_text  TEXT,
    PRIMARY KEY (run_id, key)
);

CREATE TABLE IF NOT EXISTS hotpath_functions (
    id              INTEGER PRIMARY KEY,
    run_id          INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    section         TEXT NOT NULL,
    description     TEXT,
    ordinal         INTEGER NOT NULL,
    name            TEXT NOT NULL,
    calls           INTEGER,
    avg             TEXT,
    total           TEXT,
    percent_total   TEXT,
    p50             TEXT,
    p95             TEXT,
    p99             TEXT
);

CREATE TABLE IF NOT EXISTS hotpath_threads (
    id              INTEGER PRIMARY KEY,
    run_id          INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    status          TEXT,
    cpu_percent     TEXT,
    cpu_percent_max TEXT,
    cpu_user        TEXT,
    cpu_sys         TEXT,
    cpu_total       TEXT,
    alloc_bytes     TEXT,
    dealloc_bytes   TEXT,
    mem_diff        TEXT
);

-- Create indexes
CREATE INDEX IF NOT EXISTS idx_runs_project ON runs(project);
CREATE INDEX IF NOT EXISTS idx_hotpath_functions_run_id ON hotpath_functions(run_id);
CREATE INDEX IF NOT EXISTS idx_hotpath_threads_run_id ON hotpath_threads(run_id);
CREATE INDEX IF NOT EXISTS idx_run_kv_key ON run_kv(key, run_id);
```

### Phase 2: Data migration (Rust code)

For each existing row where `extra IS NOT NULL` or `metadata IS NOT NULL`,
parse the JSON and insert into child tables. This runs in Rust because it
requires JSON parsing.

```rust
fn migrate_v2_to_v3(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // Migrate extra JSON -> child tables
    let mut stmt = conn.prepare(
        "SELECT id, extra, metadata, command FROM runs WHERE extra IS NOT NULL OR metadata IS NOT NULL"
    )?;
    let rows: Vec<(i64, Option<String>, Option<String>, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
            ))
        })?
        .filter_map(Result::ok)
        .collect();

    let mut insert_dist = conn.prepare(
        "INSERT OR IGNORE INTO run_distribution (run_id, samples, min_ms, p50_ms, p95_ms, max_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    )?;
    let mut insert_kv_int = conn.prepare(
        "INSERT OR IGNORE INTO run_kv (run_id, key, value_int) VALUES (?1, ?2, ?3)"
    )?;
    let mut insert_kv_real = conn.prepare(
        "INSERT OR IGNORE INTO run_kv (run_id, key, value_real) VALUES (?1, ?2, ?3)"
    )?;
    let mut insert_kv_text = conn.prepare(
        "INSERT OR IGNORE INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)"
    )?;
    let mut insert_fn = conn.prepare(
        "INSERT INTO hotpath_functions (run_id, section, description, ordinal, name, calls, avg, total, percent_total, p50, p95, p99) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
    )?;
    let mut insert_thread = conn.prepare(
        "INSERT INTO hotpath_threads (run_id, name, status, cpu_percent, cpu_percent_max, cpu_user, cpu_sys, cpu_total, alloc_bytes, dealloc_bytes, mem_diff) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
    )?;

    for (run_id, extra_json, metadata_json, _command) in &rows {
        // Migrate extra
        if let Some(json_str) = extra_json {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(obj) = val.as_object() {
                    migrate_extra_object(
                        *run_id, obj,
                        &mut insert_dist, &mut insert_kv_int,
                        &mut insert_kv_real, &mut insert_kv_text,
                        &mut insert_fn, &mut insert_thread,
                    )?;
                }
            }
        }
        // Migrate metadata -> run_kv with meta. prefix
        if let Some(json_str) = metadata_json {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(obj) = val.as_object() {
                    for (key, value) in obj {
                        let prefixed_key = format!("meta.{key}");
                        insert_kv_value(*run_id, &prefixed_key, value,
                            &mut insert_kv_int, &mut insert_kv_real, &mut insert_kv_text)?;
                    }
                }
            }
        }
    }

    Ok(())
}
```

The `migrate_extra_object` function handles three cases:

1. **Distribution stats** (`min_ms`, `p50_ms`, `p95_ms`, `max_ms`, `samples`
   keys present): insert into `run_distribution`, then insert remaining keys
   into `run_kv`.
2. **Hotpath data** (`functions_timing` or `functions_alloc` key present):
   parse the nested arrays and insert into `hotpath_functions` and
   `hotpath_threads`. Thread-level summary stats go into `run_kv` with
   `threads.` prefix.
3. **Plain kv pairs** (all other cases): insert each key into `run_kv`.

### Phase 3: Drop old columns

SQLite does not support `ALTER TABLE DROP COLUMN` prior to 3.35.0. Since
brokkr targets recent Linux systems with SQLite 3.35+, we can drop the old
columns. However, for maximum compatibility:

```sql
-- Keep extra and metadata columns but stop populating them.
-- They will contain NULL for all new rows.
-- This avoids the need for a table rebuild on older SQLite.
```

**Decision:** Keep `extra` and `metadata` columns in the schema but stop
reading or writing them. New code never populates them. They become dead
weight (NULL in all new rows, stale JSON in old rows). This is the safest
migration path -- no data loss, no table rebuild, no SQLite version
dependency. A future v4 migration can drop them via table rebuild when
desired.

### Phase 4: Set version

```sql
PRAGMA user_version = 3;
```

---

## What Moves Where

### Current `extra` JSON field mapping

| Source | Current `extra` key(s) | New location | Notes |
|--------|----------------------|--------------|-------|
| `run_distribution` | `min_ms`, `p50_ms`, `p95_ms`, `max_ms`, `samples` | `run_distribution` table | One-to-one mapping |
| `run_external_with_kv` (merge) | `passthrough_blobs`, `rewrite_blobs`, `passthrough_bytes`, `rewrite_bytes`, `output_bytes` | `run_kv` (integer values) | |
| `run_external_with_kv` (read) | `nodes`, `ways`, `relations`, `elapsed_ms` | `run_kv` | `elapsed_ms` from kv is already in `runs.elapsed_ms` |
| Hotpath (timing) | `functions_timing.data[*]` | `hotpath_functions` (section='timing') | |
| Hotpath (alloc) | `functions_alloc.data[*]` | `hotpath_functions` (section='alloc') | |
| Hotpath (threads) | `threads.data[*]` | `hotpath_threads` | |
| Hotpath (threads summary) | `threads.rss_bytes`, `threads.total_alloc_bytes`, etc. | `run_kv` with `threads.` prefix | |
| Planetiler read bench | `nodes`, `ways`, `relations` | `run_kv` | |
| PMTiles bench | `tiles`, `internal_runs` | `run_kv` | |
| Node store bench | `nodes_millions`, `internal_runs` | `run_kv` | |

### Current `metadata` JSON field mapping

| Source | Current `metadata` key(s) | New location |
|--------|--------------------------|--------------|
| bench_merge | `compression`, `io_mode` | `run_kv`: `meta.compression`, `meta.io_mode` |
| bench_write | `compression`, `writer_mode` | `run_kv`: `meta.compression`, `meta.writer_mode` |
| bench_read | `mode` | `run_kv`: `meta.mode` |
| bench_extract | `strategy`, `bbox` | `run_kv`: `meta.strategy`, `meta.bbox` |
| hotpath (pbfhogg) | `alloc`, `test` | `run_kv`: `meta.alloc`, `meta.test` |
| hotpath (elivagar) | `alloc`, `ocean` | `run_kv`: `meta.alloc`, `meta.ocean` |
| hotpath (nidhogg) | `alloc` | `run_kv`: `meta.alloc` |
| bench_self (elivagar) | `ocean`, `skip_to`, `compression_level` | `run_kv`: `meta.ocean`, `meta.skip_to`, `meta.compression_level` |
| bench_planetiler | `heap_mb` | `run_kv`: `meta.heap_mb` |
| bench_pmtiles | `tiles`, `internal_runs` | `run_kv`: `meta.tiles`, `meta.internal_runs` |
| bench_node_store | `nodes_millions`, `internal_runs` | `run_kv`: `meta.nodes_millions`, `meta.internal_runs` |
| bench_api (nidhogg) | `port`, `query` | `run_kv`: `meta.port`, `meta.query` |
| profile (nidhogg) | `tool` | `run_kv`: `meta.tool` |

---

## Insert Patterns

### Simple timing benchmark (bench_read, bench_commands, bench_blob_filter, bench_allocator)

```
runs:  1 row (envelope with elapsed_ms)
run_kv: 0-N rows for metadata parameters
```

Example for `bench read`:
```
INSERT INTO runs (..., command, variant, elapsed_ms, ...) VALUES (..., 'bench read', 'pipelined', 1300, ...);
-- run_id = last_insert_rowid()
INSERT INTO run_kv (run_id, key, value_text) VALUES (<id>, 'meta.mode', 'pipelined');
```

### KV-parsed benchmark (bench_merge, bench_write)

```
runs:  1 row (envelope with elapsed_ms from subprocess)
run_kv: N rows for subprocess kv pairs + M rows for metadata
```

Example for `bench merge`:
```
INSERT INTO runs (..., command, variant, elapsed_ms, ...) VALUES (..., 'bench merge', 'buffered+zlib', 3360, ...);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'passthrough_blobs', 4200);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'rewrite_blobs', 504);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'passthrough_bytes', 420000000);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'rewrite_bytes', 45000000);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'output_bytes', 465000000);
INSERT INTO run_kv (run_id, key, value_text) VALUES (<id>, 'meta.compression', 'zlib');
INSERT INTO run_kv (run_id, key, value_text) VALUES (<id>, 'meta.io_mode', 'buffered');
```

### Distribution benchmark (bench_api)

```
runs:             1 row (envelope with min elapsed_ms)
run_distribution: 1 row (min/p50/p95/max/samples)
run_kv:           M rows for metadata
```

Example for `bench api`:
```
INSERT INTO runs (..., command, variant, elapsed_ms, ...) VALUES (..., 'bench api', 'cph_highways', 42, ...);
INSERT INTO run_distribution (run_id, samples, min_ms, p50_ms, p95_ms, max_ms)
    VALUES (<id>, 10, 42, 45, 52, 58);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'meta.port', 8080);
INSERT INTO run_kv (run_id, key, value_text) VALUES (<id>, 'meta.query', 'cph_highways');
```

### Hotpath profiling

```
runs:              1 row (envelope with wall-clock ms)
hotpath_functions: 10-30 rows (timing section)
hotpath_functions: 10-30 rows (alloc section, if alloc=true)
hotpath_threads:   3-8 rows
run_kv:            5-10 rows (thread summary stats + metadata)
```

Example:
```
INSERT INTO runs (..., command, variant, elapsed_ms, ...) VALUES (..., 'hotpath', 'merge', 5200, ...);

-- functions_timing
INSERT INTO hotpath_functions (run_id, section, description, ordinal, name, calls, avg, total, percent_total, p50, p95, p99)
    VALUES (<id>, 'timing', 'wall clock', 0, 'decompress_blob', 4704, '205 ns', '8.81 s', '42.3%', '180 ns', '450 ns', '1.2 µs');
INSERT INTO hotpath_functions (run_id, section, description, ordinal, name, ...)
    VALUES (<id>, 'timing', 'wall clock', 1, 'add_way', ...);
-- ... more function rows ...

-- threads
INSERT INTO hotpath_threads (run_id, name, status, cpu_percent, cpu_percent_max, cpu_user, cpu_sys, cpu_total, ...)
    VALUES (<id>, 'main', 'running', '95.2%', '98.1%', '4.8 s', '0.3 s', '5.1 s', ...);

-- thread summary stats
INSERT INTO run_kv (run_id, key, value_text) VALUES (<id>, 'threads.rss_bytes', '1.2 GB');
INSERT INTO run_kv (run_id, key, value_text) VALUES (<id>, 'threads.total_alloc_bytes', '3.4 GB');
INSERT INTO run_kv (run_id, key, value_text) VALUES (<id>, 'threads.total_dealloc_bytes', '3.1 GB');
INSERT INTO run_kv (run_id, key, value_text) VALUES (<id>, 'threads.alloc_dealloc_diff', '300 MB');

-- metadata
INSERT INTO run_kv (run_id, key, value_text) VALUES (<id>, 'meta.alloc', 'false');
INSERT INTO run_kv (run_id, key, value_text) VALUES (<id>, 'meta.test', 'merge');
```

### Elivagar self benchmark

```
runs:  1 row (envelope with total_ms as elapsed_ms)
run_kv: N rows for phase timings + metrics + metadata
```

Example:
```
INSERT INTO runs (..., command, variant, elapsed_ms, ...) VALUES (..., 'bench self', NULL, 45000, ...);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'phase12_ms', 12000);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'ocean_ms', 3000);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'phase3_ms', 15000);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'phase4_ms', 15000);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'features', 5000000);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'tiles', 120000);
INSERT INTO run_kv (run_id, key, value_int) VALUES (<id>, 'output_bytes', 300000000);
INSERT INTO run_kv (run_id, key, value_text) VALUES (<id>, 'meta.ocean', 'true');
```

---

## Query Patterns

### 1. UUID prefix lookup

```sql
SELECT r.*, d.samples, d.min_ms, d.p50_ms, d.p95_ms, d.max_ms
FROM runs r
LEFT JOIN run_distribution d ON d.run_id = r.id
WHERE r.uuid LIKE ?1||'%'
ORDER BY r.id DESC;

-- Then fetch kv pairs:
SELECT key, value_int, value_real, value_text FROM run_kv WHERE run_id = ?1;

-- Then fetch hotpath data (if command = 'hotpath'):
SELECT * FROM hotpath_functions WHERE run_id = ?1 ORDER BY section, ordinal;
SELECT * FROM hotpath_threads WHERE run_id = ?1;
```

This is a two-round-trip pattern but each query is index-backed and fast.
The current code already does multiple rounds (parse JSON, format hotpath).

### 2. Recent results listing (default `brokkr results`)

```sql
SELECT r.id, r.uuid, r.timestamp, r.[commit], r.command, r.variant,
       r.elapsed_ms, r.input_file, r.input_mb, r.peak_rss_mb
FROM runs r
WHERE r.project = ?1
ORDER BY r.id DESC
LIMIT ?2;
```

Same as today, but with optional `peak_rss_mb` column in the table output.
The `project` filter ensures elivagar runs do not clutter pbfhogg results.

### 3. Commit prefix filter

```sql
SELECT ...
FROM runs r
WHERE r.[commit] LIKE ?1||'%'
  AND r.project = ?2
ORDER BY r.id DESC
LIMIT ?3;
```

### 4. Side-by-side comparison (`--compare A B`)

```sql
-- Fetch rows for commit A
SELECT r.id, r.command, r.variant, r.input_file, r.input_mb, r.elapsed_ms,
       r.peak_rss_mb
FROM runs r
WHERE r.[commit] LIKE ?1||'%'
  AND r.project = ?2
  [AND r.command = ?3]
  [AND r.variant LIKE ?4||'%']
ORDER BY r.command, r.variant, r.id DESC;

-- Same for commit B

-- For output_bytes comparison, fetch from run_kv:
SELECT value_int FROM run_kv
WHERE run_id = ?1 AND key = 'output_bytes';
```

The comparison table gains a `peak_rss_mb` column alongside `elapsed_ms`.
The hotpath diff is built from `hotpath_functions` rows instead of parsing
JSON:

```sql
-- Functions for run A
SELECT section, name, total, percent_total FROM hotpath_functions
WHERE run_id = ?1 ORDER BY section, ordinal;

-- Functions for run B
SELECT section, name, total, percent_total FROM hotpath_functions
WHERE run_id = ?2 ORDER BY section, ordinal;
```

### 5. Compare last (`--compare-last`)

```sql
SELECT DISTINCT [commit] FROM runs
WHERE project = ?1
  [AND command = ?2]
  [AND variant LIKE ?3||'%']
ORDER BY id DESC
LIMIT 2;
```

Then proceed as in pattern 4.

### 6. Future: query by kv metadata

```sql
-- Find all merge benchmark runs with zlib compression
SELECT r.* FROM runs r
JOIN run_kv kv ON kv.run_id = r.id
WHERE r.command = 'bench merge'
  AND kv.key = 'meta.compression'
  AND kv.value_text = 'zlib'
ORDER BY r.id DESC;
```

This is a new capability not possible with the current schema without
JSON extraction.

---

## Extensibility Story

### Adding elivagar-specific metrics

Elivagar already uses the harness to store results. With the new schema:

- Phase timings (`phase12_ms`, `ocean_ms`, `phase3_ms`, `phase4_ms`) go into
  `run_kv` as integer values. No schema change needed.
- Tile counts, dedup rates, feature counts go into `run_kv`.
- If elivagar needs a dedicated structured table (e.g., per-zoom-level tile
  counts), add a new child table with a `run_id` foreign key. No changes to
  existing tables.

The `project` column on `runs` enables `brokkr results --project elivagar`
filtering.

### Adding nidhogg-specific metrics

Same pattern. Ingest timing is already stored. Query latency distributions
are in `run_distribution`. Query metadata (bbox, filter expression) in
`run_kv`.

### Future memory optimization experiments

The upcoming per-phase RSS sampling work needs:

- **Per-phase timing**: `run_kv` with keys like `phase.decode_ms`,
  `phase.rewrite_ms`, `phase.write_ms`.
- **Per-phase RSS**: `run_kv` with keys like `phase.decode_rss_mb`,
  `phase.rewrite_rss_mb`.
- **Blob-size distributions**: a future `run_blob_stats` child table if
  needed, or `run_kv` with keys like `blob_size.p50`, `blob_size.p95`.

None of these require schema changes -- `run_kv` absorbs them all.

### Adding a completely new project

1. Register the project in `brokkr.toml`.
2. Set `project = "new_project"` on `BenchConfig` or equivalent.
3. All existing infrastructure (harness, DB, results display) works.
4. If the project needs structured child tables, add them with `run_id`
   foreign keys.

---

## Size and Performance Implications

### Database size

**Current:** A typical pbfhogg database has ~500-2000 rows in `runs`. Each
row is ~500 bytes (columns) + ~200 bytes (JSON in `extra`) + ~100 bytes
(JSON in `metadata`). Total: ~800 bytes/row, ~1.5 MB for 2000 rows.

**After migration:** The `runs` table shrinks slightly (no JSON columns for
new rows). Child tables add rows:

- `run_distribution`: ~1 row per distribution benchmark (rare). 36 bytes/row.
- `run_kv`: ~5-10 rows per run for metadata + subprocess metrics. ~60 bytes/row.
  For 2000 runs with 8 kv pairs each: 16,000 rows, ~960 KB.
- `hotpath_functions`: ~20 rows per hotpath run. With ~200 hotpath runs:
  4,000 rows, ~400 KB.
- `hotpath_threads`: ~5 rows per hotpath run. ~1,000 rows, ~80 KB.

**Total estimated size after migration:** ~3 MB for a database with 2000 runs.
This is approximately 2x the current size, entirely due to the denormalization
of JSON into rows. Acceptable for a database that is committed to git.

### Query performance

**Improved queries:**

- UUID lookup: same speed (index-backed).
- Comparison: faster for hotpath diffs (no JSON parsing at display time).
- Metadata filtering: new capability, index-backed via `idx_run_kv_key`.
- Distribution stats: direct column access instead of JSON extraction.

**Slightly slower queries:**

- Detailed single-row display: now requires 2-3 additional queries (kv,
  functions, threads) instead of reading one JSON blob. Each query is
  index-backed and sub-millisecond on SQLite. Net effect: imperceptible.

**WAL mode** continues to be used. No contention concerns (single-writer,
benchmarks run sequentially).

### Git repository impact

The `.brokkr/results.db` file grows by ~1.5 MB due to the migration (old
rows retain `extra`/`metadata` columns; new child tables add data). Future
growth rate stays similar because new rows have NULL in `extra`/`metadata`
and child table rows are roughly the same total bytes. SQLite's page-based
storage means git diffs remain efficient (only changed pages are stored).

---

## Rust Implementation Notes

### `RunRow` changes

```rust
pub struct RunRow {
    pub hostname: String,
    pub commit: String,
    pub subject: String,
    pub command: String,
    pub variant: Option<String>,
    pub input_file: Option<String>,
    pub input_mb: Option<f64>,
    pub elapsed_ms: i64,
    pub peak_rss_mb: Option<f64>,         // NEW
    pub cargo_features: Option<String>,
    pub cargo_profile: String,
    pub kernel: Option<String>,
    pub cpu_governor: Option<String>,
    pub avail_memory_mb: Option<i64>,
    pub storage_notes: Option<String>,
    pub cli_args: Option<String>,
    pub project: String,                   // NEW
    pub kv: Vec<KvPair>,                   // NEW: replaces extra + metadata
    pub distribution: Option<Distribution>,// NEW: replaces extra distribution fields
    pub hotpath: Option<HotpathData>,      // NEW: replaces extra hotpath JSON
}

pub struct KvPair {
    pub key: String,
    pub value: KvValue,
}

pub enum KvValue {
    Int(i64),
    Real(f64),
    Text(String),
}

pub struct Distribution {
    pub samples: i64,
    pub min_ms: i64,
    pub p50_ms: i64,
    pub p95_ms: i64,
    pub max_ms: i64,
}

pub struct HotpathData {
    pub functions: Vec<HotpathFunction>,
    pub threads: Vec<HotpathThread>,
    pub thread_summary: Vec<KvPair>,
}
```

### `StoredRow` changes

```rust
pub struct StoredRow {
    // ... existing fields ...
    pub peak_rss_mb: Option<f64>,         // NEW
    pub project: String,                   // NEW
    // extra and metadata removed -- replaced by lazy-loaded child data
}
```

Child data is loaded on demand (not for every query). The `query_by_uuid`
path loads all children. The `query` listing path loads only the envelope.
The `query_compare` path loads envelopes + hotpath functions (for diff).

### `BenchResult` changes

```rust
pub struct BenchResult {
    pub elapsed_ms: i64,
    pub peak_rss_mb: Option<f64>,          // NEW
    pub kv: Vec<KvPair>,                   // replaces extra JSON
    pub distribution: Option<Distribution>,// replaces extra distribution fields
    pub hotpath: Option<HotpathData>,      // replaces extra hotpath JSON
}
```

### Peak RSS collection

For subprocess benchmarks (`run_external`, `run_external_with_kv`):
1. After the child process exits, read `/proc/<pid>/status` for `VmHWM`
   (peak resident set size).
2. On Linux, this requires reading before the process is reaped. Use
   `std::process::Command` with `.spawn()` + `.wait()` and read
   `/proc/<pid>/status` between wait completion and reap (or just accept
   the race -- VmHWM is available until the pid is recycled).
3. Alternatively, use `getrusage(RUSAGE_CHILDREN)` after `waitpid` to get
   `ru_maxrss` (in KB on Linux). This is simpler and race-free.

For internal benchmarks (`run_internal`): read `/proc/self/status` for
`VmHWM` after the closure returns.

### `insert()` changes

The `insert` method becomes a transaction that inserts the envelope row,
then inserts child rows:

```rust
pub fn insert(&self, row: &RunRow) -> Result<String, DevError> {
    let uuid = generate_uuid()?;
    self.conn.execute("BEGIN", [])?;

    // Insert envelope
    self.conn.execute(INSERT_SQL, /* params */)?;
    let run_id = self.conn.last_insert_rowid();

    // Insert distribution (if present)
    if let Some(ref dist) = row.distribution {
        self.conn.execute(INSERT_DIST_SQL, params![run_id, dist.samples, ...])?;
    }

    // Insert kv pairs
    for kv in &row.kv {
        match &kv.value {
            KvValue::Int(v) => self.conn.execute(INSERT_KV_INT, params![run_id, kv.key, v])?,
            KvValue::Real(v) => self.conn.execute(INSERT_KV_REAL, params![run_id, kv.key, v])?,
            KvValue::Text(v) => self.conn.execute(INSERT_KV_TEXT, params![run_id, kv.key, v])?,
        };
    }

    // Insert hotpath data (if present)
    if let Some(ref hp) = row.hotpath {
        for (i, func) in hp.functions.iter().enumerate() {
            self.conn.execute(INSERT_FN_SQL, params![run_id, func.section, ...])?;
        }
        for thread in &hp.threads {
            self.conn.execute(INSERT_THREAD_SQL, params![run_id, thread.name, ...])?;
        }
    }

    self.conn.execute("COMMIT", [])?;
    Ok(short_uuid(&uuid))
}
```

---

## Migration Ordering

The migration runs in `run_migrations()` when `PRAGMA user_version < 3`:

```rust
const SCHEMA_VERSION: i64 = 3;

fn run_migrations(conn: &rusqlite::Connection) -> Result<(), DevError> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    if current < 1 {
        migrate_uuid(conn)?;       // existing v0->v1
    }
    if current < 2 {
        migrate_cli_args_metadata(conn)?;  // existing v1->v2
    }
    if current < 3 {
        migrate_v2_to_v3(conn)?;   // NEW
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}
```

The v2->v3 migration is idempotent: `CREATE TABLE IF NOT EXISTS` and
`INSERT OR IGNORE` ensure re-running is safe. The `ALTER TABLE ADD COLUMN`
calls check for column existence first (same pattern as existing migrations).

---

## Summary of Changes

| Aspect | v2 (current) | v3 (proposed) |
|--------|-------------|---------------|
| Tables | 1 (`runs`) | 5 (`runs`, `run_distribution`, `run_kv`, `hotpath_functions`, `hotpath_threads`) |
| JSON columns | 2 (`extra`, `metadata`) | 0 |
| Peak RSS | Not stored | `runs.peak_rss_mb` |
| Distribution stats | JSON in `extra` | `run_distribution` table |
| Subprocess kv | JSON in `extra` | `run_kv` table |
| Benchmark params | JSON in `metadata` | `run_kv` with `meta.` prefix |
| Hotpath functions | JSON in `extra` | `hotpath_functions` table |
| Hotpath threads | JSON in `extra` | `hotpath_threads` table |
| Multi-project | Implicit (separate DBs) | `runs.project` column |
| Schema version | 2 | 3 |
| Estimated DB size | ~1.5 MB / 2000 runs | ~3 MB / 2000 runs |
