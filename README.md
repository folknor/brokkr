# brokkr

Shared command orchestrator and development utility for a family of Rust and data projects. A single Rust binary provides benchmarking, verification, profiling, visual reference testing, and operational commands across those projects.

Built with LLMs. See [LLM.md](LLM.md).


## Install

```
cargo install --path ~/Programs/brokkr
```

## How it works

Run `brokkr` from any project root. It reads `./brokkr.toml` to detect the project and resolve datasets, paths, and host-specific configuration. Project-specific commands are gated by that detected project, so a litehtml command such as `prepare` from pbfhogg's root produces a clear error. Shared Rust tooling such as `check` also works in any Rust+git repo, with or without `brokkr.toml`.

```
cd ~/Programs/pbfhogg
brokkr bench                                      # list script benches
brokkr bench NAME                                 # run a declared bench

cd ~/Programs/litehtml-rs
brokkr visual --all                                 # visual reference tests
```

## Commands

### Measurement

Every project measures through script benches: `brokkr bench NAME` runs a bench the project declares, with `--hotpath` and `--alloc` for profiled runs and `--commit` for older commits (`brokkr man bench`). Every measured child gets a sidecar that samples `/proc` metrics at 100ms (see [Sidecar profiler](#sidecar-profiler) below). The typed measured commands are retired; the history they stored in `.brokkr/results.db` and `.brokkr/sidecar.db` stays readable.

### Shared (all projects)

| Command | Description |
|---------|-------------|
| `check` | Run configured conventions, rustdoc and clippy checks, and test sweeps (extra cargo arguments are forwarded) |
| `test [-p <PKG>] <NAME>` | Run matching cargo tests with ignored tests included; profile comes from CLI flags, the selected sweep, and `[test] debug`. Package selection uses `-p`, `[test] default_package`, then the project's built-in default. `-N` repeats; `-j` sets cargo's compile parallelism. |
| `env` | Show hostname, kernel, governor, memory, drives, the free space of the data, scratch and target filesystems (never walked for size), tool versions, dataset status, and brokkr's own state: each store's allocated size, the free space of each filesystem holding one, and any filesystem under 10% free |
| `results` | Query the results database (`.brokkr/results.db`) |
| `invalidate` | Hard-delete results + sidecar rows by UUID or commit prefix (dry-run unless `-f`) |
| `clean [--worktrees]` | Remove scratch/temp files; `--worktrees` also purges persistent benchmark worktrees |
| `history` | Browse global command history |
| `lock` | Show who holds the benchmark lock, and the live serves |
| `serve [TARGET]` | Bare: list the live background servers. `TARGET`: build it as `run` does and serve it in the background without holding the lock; measurements refuse while it lives. `--stop [WHICH]` stops one (`brokkr man serve`) |

`check` filters cargo output into one line per diagnostic. Compilation noise is stripped; each error or warning becomes `error[CODE] file:line:col message` or `warning[rule] file:line:col message`. Passing tests are aggregated (for example, `cargo test: N passed (M suites, elapsed)`), failures become `FAILED name location message`. There is no unfiltered mode - the one-line form is the whole surface - though a parse failure still falls back to dumping the raw streams, since a stream brokkr could not read is the one case where the unprocessed text is all there is. `--json` appends a single machine-readable summary line.

A gremlin scan runs as part of the configured convention checks and fails the check if any banned Unicode character is found in a tracked file with a built-in source, config or docs extension, or one a `[gremlins].include` glob names (`brokkr man check gremlins` lists the extensions). Covers invisible/zero-width characters, non-breaking spaces, soft hyphen, line/paragraph separators, bidi marks/overrides/isolates, em/en dashes, typographic single and double quotes, emoji and pictographs, plus `U+0003`, `U+000B`, and `U+FFFC`. Text mode prints one line per hit (`file:line:col U+XXXX NAME`); JSON mode includes the findings in the command summary. See `src/gremlins.rs` for the full banned set.

If `brokkr.toml` contains `[[dependency_rule]]` entries, `check` then validates direct Cargo dependency boundaries from `cargo metadata --no-deps`. Example:

```toml
[[dependency_rule]]
name = "app-db-boundary"
from = "app"
forbid = ["db", "service-state"]
```

This rejects direct `app -> db` or `app -> service-state` dependencies before clippy/tests run. `from` and `forbid` each accept either one string or an array of package names; `forbid` can name workspace crates or external crates. Use `allow = [...]` instead of `forbid` for the inverse polarity: any direct dependency not listed is a violation (the right shape for a leaf crate).

Every diagnostic is an error, except a warning from a dependency outside the workspace. When a phase finds errors, text mode lists the ones in files with unstaged changes first, then the rest. Nothing is capped and there is no flag to raise a cap: every error prints, every time - in the one-line form, which is the only form.

`check` runs clippy and tests against the same list of "active sweeps" - one cargo invocation per sweep, both phases. The sweep list is built from `brokkr.toml`, in priority order:

1. CLI `--features X` / `--no-default-features` → a single ad-hoc sweep with those exact flags. Useful for spot-checking one combo without editing `brokkr.toml`. Skips `[[check]]` and any profile entirely; no `build_packages`.
2. CLI `--profile <name>` or `[test].default_profile` → the named profile's resolved sweep list, with the profile's libtest filters (`only` / `skip` / `include_ignored` / `test_threads` / `env`) applied to the test phase. Each sweep references a `[[check]]` entry by name.
3. `[[check]]` is configured but no profile applies → every `[[check]]` entry runs in declaration order, no libtest filters.
4. None of the above → one `--all-features` sweep, identical to `check`'s pre-`[[check]]` behaviour. Projects that haven't migrated keep working unchanged.

Each `[[check]]` entry declares `name`, optional `features = [...]` (explicit list - the `features = "all"` sentinel is rejected), optional `no_default_features`, and optional `build_packages = [...]`. `build_packages` rebuilds those cargo packages with the entry's feature flags before the test phase, so `tests/cli_*.rs` integration tests don't silently invoke a stale binary built for a different feature set. Diagnostics from clippy and rustdoc are deduped across sweeps. A line is tagged with the sweeps that reported it (`[<sweep-name>]`, `[a+b]`) only when some sweep that selected its package did not - a diagnostic every covering sweep reports belongs to the code, not to a build shape. Cost is ~Nx cold (cargo keeps separate `target/` per feature set); incremental is small.

`check` also works without a `brokkr.toml`, so you can drop it into any Rust+git repo and get the default convention, rustdoc, clippy, and test pipeline (single `--all-features` sweep, no profiles).

`test [-p <PKG>] <NAME>` is the narrow counterpart to `check` for drilling into one specific test. It invokes cargo with the selected package and name as a substring filter, so unit and integration tests in that package can match. Package resolution runs in order: explicit `-p/--package`, `[test] default_package` in `brokkr.toml`, then the project's built-in default. Multi-crate workspaces without a built-in default can set `default_package` or pass `-p`. The build profile follows CLI overrides, the selected sweep's profile, and `[test] debug` (release when none selects dev). The runner includes ignored tests and captures their output; each harness gets a `[test]` result with wall time and failure details. Sweep selection follows `[test].default_profile`, configured `[[check]]` entries, or a single `--all-features` sweep. Profile-level libtest filters are not applied because the user's `<NAME>` is the filter. `build_packages` are rebuilt with matching feature flags before the test phase. `SKIP` means a sweep found no matching test; the whole command succeeds when at least one sweep runs a match. `-N <n>` repeats the test per sweep, and `-j <n>` sets cargo's compile parallelism.

### pbfhogg

Run declared script benches with `brokkr bench NAME`. Answer checking runs
through `brokkr run --with pbfhogg pbfhogg-verify -- CHECK`; dataset download
and snapshot promotion use `pbfhogg-datasets`, and library verification uses
`pbfhogg-library`. See `docs/projects/pbfhogg.md`. Historical typed-command
results remain readable.

### elivagar

brokkr has no elivagar commands. elivagar keeps its tooling in-tree: its own crates (the corpus gate, `elivagar-devkit`, datasets, and `elivagar-tools`, which installs the competitor tools) run through the generic `run`, and it measures through script benches (`brokkr bench`, including `brokkr bench planetiler` and `brokkr bench tilemaker`). Its host keys (`[<host>.tilegen.<name>]`, `output`, and the PBF entry's `locations_on_ways` and `force_sorted`) still load, and its script benches read them from the resolved view. Historical typed-command results remain readable.

### nidhogg

brokkr has no nidhogg commands. nidhogg serves, ingests, updates and pokes through its own binaries run by the generic `run` (`brokkr run --with nidhogg nidhogg-serve -- ...`, `brokkr run nidhogg-update -- ...`), measures through script benches (`brokkr bench`), and validates with `brokkr check`.

### litehtml-rs

Visual reference testing and fixture preprocessing. All commands are top-level (no `brokkr litehtml` namespace). Shared visual testing commands (`visual`, `list`, `approve`, `report`, `visual-status`) dispatch to litehtml or sluggrs based on the detected project. `visual` was formerly named `test`; that name is now owned by the generic cargo single-test runner in the Shared table above.

| Subcommand | Description |
|------------|-------------|
| `visual` | Run fixtures against Chrome reference artifacts (pixel diff + element comparison) |
| `list` | Show fixtures, tags, and approval state |
| `approve` | Record current divergence as accepted baseline |
| `visual-status` | Dashboard of all fixtures vs approved baselines |
| `report` | Show results for a past test run |
| `prepare` | Normalize raw email HTML into self-contained fixture (images → gray PNGs, inject Ahem font, strip external resources, pretty-print) |
| `html-extract` | Extract sub-fixture by CSS selector (`--selector`) or sibling range (`--from`/`--to`) |
| `outline` | Structural overview with section markers, content previews, and suggested selectors |

**Fixture workflow**:
```
brokkr prepare raw-email.html fixtures/email-prepared.html
brokkr outline fixtures/email-prepared.html --selectors
brokkr html-extract fixtures/email-prepared.html \
  --from "div:nth-of-type(2) > table > tbody > tr > td > div:nth-of-type(4) > div" \
  --to   "div:nth-of-type(2) > table > tbody > tr > td > div:nth-of-type(7) > div" \
  fixtures/creatine_products.html
brokkr visual creatine_products
```

`prepare` and `html-extract` shell out to a Node.js script (requires Node + pnpm; auto-installs dependencies on first use). Node is already required for Puppeteer-based Chrome capture.

## Sidecar profiler

Every child a script bench measures (timing, hotpath, alloc) is sampled from `/proc/{pid}/stat`, `/proc/{pid}/io`, and `/proc/{pid}/status` at 100ms intervals, and the data is stored with the run in the bench store. The typed runs' trajectories stay in `.brokkr/sidecar.db` (gitignored - local to the machine that ran it).

The child process receives `BROKKR_MARKER_FIFO` env var pointing to a named pipe for application phase markers and counters. Markers are lines of the form `<timestamp_us> <name>`, counters are `<timestamp_us> @<name>=<value>`, every timestamp in microseconds since the spawned process started, which lines up approximately with the sample clock (not UNIX epoch; "The clock" in `brokkr man measure`); a harness running several calls in one process tags either with its call, `<timestamp_us> %<call_id> ...`, and `brokkr sidecar <uuid> --call <call_id>` reads one call's stream (see "Marker & counter rules" in `brokkr man measure`; a tagged body starting `@@` or `!` is reserved for a call sink's sealing and terminal records). Markers are point-in-time bookmarks - the protocol has no notion of spans or pairs. `--durations` and `--phase` read pair spans from a `FOO_START` / `FOO_END` convention if the emitter happens to follow it; nothing else assumes that structure.

A stop marker kills the child process as soon as the named untagged marker is emitted, allowing benchmarks of individual phases without waiting for the full run to complete; a script bench asks for one per call. The SIGKILL exit is treated as success.

Sidecar data is stored even when the child is OOM-killed - the `/proc` trajectory up to the kill is the most valuable use case.

### Querying sidecar data

A UUID prefix is required (except for `--compare`) - use `brokkr results` to find one. The `dirty` pseudo-UUID resolves to the most recent typed run that filed no results row; each earlier one keeps its own id.

```
brokkr sidecar <uuid>                              # per-phase summary (default view)
brokkr sidecar <uuid> --human                      # same, as a fixed-width table
brokkr sidecar <uuid> --samples                    # raw /proc samples (JSONL)
brokkr sidecar <uuid> --samples --fields rss,anon --every 10  # project + downsample
brokkr sidecar <uuid> --samples --where "majflt>0" --tail 20  # filter + range
brokkr sidecar <uuid> --samples --phase STAGE2                # filter to a marker phase
brokkr sidecar <uuid> --samples --range 10.0..82.0            # time window filter
brokkr sidecar <uuid> --markers                    # raw marker events (JSONL)
brokkr sidecar <uuid> --durations                  # START/END pair timings
brokkr sidecar <uuid> --counters                   # application counters
brokkr sidecar <uuid> --stalls                     # *_wait_ns counter stall attribution (see conventions below)
brokkr sidecar <uuid> --stat anon                  # min/max/avg/p50/p95 for a field
brokkr sidecar <uuid> --stat anon --phase STAGE2   # per-phase stat
brokkr sidecar --compare <uuid_a> <uuid_b>         # phase-aligned comparison
brokkr sidecar dirty --stat anon                   # inspect last failed/dirty run
brokkr sidecar <bench-uuid> --run 2 --child 0      # a script-bench run: one child of an iteration
brokkr sidecar --compare <uuid_a>/0/1 <uuid_b>     # a bench side may name ITER/CHILD
```

A script-bench run's uuid works too: each view reads one child of each selected iteration (`--run`, `--child`), its interval known from spawn to wall ("Script-bench children" in `brokkr man measure`).

Filter flags (`--phase`, `--range`, `--where`) compose with `--samples` and `--stat`; `--phase` also composes with `--counters`. `--grep <SUBSTR>` filters `--counters` by counter name, and composes with `--phase`.

### Sidecar conventions

The FIFO protocol is convention-free - brokkr doesn't mandate any naming scheme on markers or counters. Two optional conventions unlock richer views when emitters adopt them:

- **`FOO_START` / `FOO_END` marker pairs.** Used by `--durations` to derive per-span timing. Brokkr pairs a `FOO_START` with the next `FOO_END` of the same base name; unpaired starts render as standalone markers. In `--human`, span names seen more than once collapse into a single aggregate row (`NAME xN  total …  min/avg/max`) so a high-frequency span can't bury the phase rows; the JSONL form stays one object per span. The split is on observed cardinality, not on the name. A stop at `FOO_END` kills the child on the end marker verbatim. **Markers are for the small set of true phase boundaries only** - a high-frequency span emitted as marker pairs drowns every phase-oriented view at once (each marker is treated as a boundary), so per-event blocking spans belong in counters, not markers (see below).

- **`<category>_wait_ns` stall counters.** Accumulated blocking time is a *counter* concept, not a marker one. Emit a strictly-monotonic counter named `<category>_wait_ns` - one atomic add of the blocked nanoseconds per blocking event (channel sends, mutex waits, I/O backpressure, one-shot joins). `brokkr sidecar <uuid> --stalls` takes the max value per name (monotonic, so max is the cumulative total), strips `_wait_ns` for the category, and reports each as ms + `% of wall`. The `%` can exceed 100 for a counter accumulated across concurrent threads - it's the average number of threads parked in that category at any instant (e.g. 430% ~= 4.3 threads in decode-send); single-threaded waits read as clean sub-100% fractions. The view spans projects: it rolls up pbfhogg's `pipeline_decoded_send_wait_ns`, `pipeline_raw_send_wait_ns`, ... alongside elivagar's `sort_chunk_write_wait_ns`, `assemble_partition_batch_wait_ns`, `pmtiles_write_wait_ns`, ... in one table. Runs with no `*_wait_ns` counters produce a clear "no *_wait_ns counters" message rather than silent empty output.

Both conventions are additive - projects can opt in incrementally. Existing emissions keep working with no change, and the only required mechanical migration is whichever blocking points you want to instrument. (Historical note: `--stalls` originally paired `WAIT_<CATEGORY>_START`/`_END` markers; it moved to `*_wait_ns` counters so the marker stream stays reserved for phase boundaries and the stall view unifies with pbfhogg's existing wait counters. Brokkr no longer treats any `WAIT_`-prefixed marker specially.)

The marker and counter conventions are described here as the durable contract; [`LLM.md`](LLM.md) has broader project history.

## Results database

Query stored benchmarks with `brokkr results`:

```
brokkr results                                      # table of last 20 results
brokkr results 0b74fb6f                             # look up by UUID prefix
brokkr results --command read                       # last 20 matching 'read'
brokkr results --commit a65a                        # filter by commit
brokkr results --mode hotpath                       # filter by measurement mode
brokkr results --grep pipelined                     # substring-match cli_args + brokkr_args
brokkr results --grep apply-changes --grep uring --grep zstd:1   # stack --grep: AND across terms
brokkr results --grep apply-changes --grep-v uring  # exclude: the arm WITHOUT io_uring
brokkr results --dataset europe                     # filter by dataset (substring on input file)
brokkr results --command tags-filter --dataset eu   # combine filters
brokkr results --meta merged_cache=miss             # filter by runtime-observation metadata key
brokkr results --command diff-snapshots --grep 'format osc'     # osc-only diff-snapshots runs
brokkr results --meta merged_cache=miss --command diff           # cold-cache diff runs only
brokkr results --compare a65a 911c                  # compare two commits side-by-side
```

Columns that drive the filters:

- `command` - the bare subcommand id (`read`, `cat`, `diff-snapshots`, `tilegen`, … on historical rows); no `bench `/`hotpath ` prefix.
- `mode` - the measurement mode only (`bench` / `hotpath` / `alloc`).
- `cli_args` - literal subprocess argv. Every flag and axis the user passed is here verbatim.
- `brokkr_args` - literal `brokkr <...>` invocation the user typed. Same-column-dual-view, recorded so `--grep` can find either.
- `meta.*` (via `--meta KEY=VALUE`) - runtime observations only: resolved paths, detected modes, cache hit/miss. Anything derivable from `cli_args` or `brokkr_args` is NOT duplicated here. `meta.` prefix is implicit; multiple `--meta` flags AND together; rows missing the requested key are silently excluded. Available keys depend on the command - e.g. historical pbfhogg `diff` rows carry `meta.merged_cache` + `meta.merged_cache_age_s`, and historical elivagar `tilegen` rows `meta.locations_on_ways_detected`; historical `meta.format`/`meta.index_type`/`meta.start_stage` keys were migrated out in v13 (they live in `cli_args` now).

Use `--grep` for anything that's a flag/axis (`--grep zstd:1`, `--grep 'snapshot 20260411'`, `--grep 'index-type external'`). Use `--meta` for genuine runtime observations.

`--grep-v` is the negative form: repeatable, and a row is excluded if it matches **any** term (`--grep` ANDs, `--grep-v` ORs). It's the only way to select an A/B arm defined by an *absent* flag - `--grep uring` finds the ON arm, but the OFF arm has no distinguishing token to match, so it needs `--grep-v uring`. Both are literal substring matches (`%` and `_` are not wildcards), and both apply to `--compare` as well as the row listing.

`brokkr results <uuid>` additionally shows what a table row can't: the per-iteration walls of a `--bench N` run in execution order, and the `prev.*` pairs naming what ran immediately before. `--compare A B` annotates a pair with a `host:` line when the two runs saw different available memory, governor, or kernel.

The `dataset` column in the output table is the first dash-separated component of the input filename - `europe-20260301-seq4714-with-indexdata.osm.pbf` renders as `europe`. This is a display heuristic: filtering via `--dataset` always substring-matches the full `input_file` column, so filters still work even when the short name collapses distinct datasets (e.g. a hypothetical `europe-west` would display as `europe`). The full filename and size are shown in the single-result detail view (`brokkr results <uuid>`) as the `input` field.

The compare view shows timing, output size, peak RSS, rewrite ratio, and blob distribution columns as applicable. Hotpath comparisons include function-level timing diffs.

### Invalidating results

A benchmark run under the wrong pretences (wrong dataset, wrong features, wrong git state, interrupted sidecar, …) produces numbers that will skew every future comparison. `brokkr invalidate` hard-deletes such runs from both `.brokkr/results.db` (runs + `run_distribution` + `run_iterations` + `run_kv` + `hotpath_functions` + `hotpath_threads`; every row of a library measurement's group, which shares one UUID; the content-addressed `library_documents` stay) and `.brokkr/sidecar.db` (samples, markers, summary, counters, sink records, call boundaries, meta, and any `sidecar_latest` pointer rows like `dirty` that resolve to a deleted UUID).

Dry-run by default - the command prints each matched UUID and tags whether sidecar data is present, then exits. Pass `-f` / `--force` to actually delete.

```
brokkr invalidate 0b74fb6f              # preview by UUID prefix
brokkr invalidate 0b74fb6f -f           # perform
brokkr invalidate --commit a65a         # preview every run on matching commits
brokkr invalidate --commit a65a -f      # perform
brokkr invalidate dirty -f              # nuke the last dirty/failed run's sidecar
```

Sidecar-only UUIDs (dirty-tree or failed runs with no results DB row) are picked up too, so a single invalidate call cleans both sides. Deletions in `.brokkr/results.db` should be committed like any other change to that file - git history preserves the audit trail.

## Quick runtime timing

For a one-off run with no storage, use `brokkr run NAME -- <args>`.

## Configuration

Each project has a `brokkr.toml` in its root:

```toml
project = "pbfhogg"

# Host-specific config (matched by hostname)
[plantasjen]
data = "data"
scratch = "data/scratch"
target = "target"
drives.source = "nvme"
drives.data = "ssd"
features = ["linux-direct-io", "linux-io-uring"]

[plantasjen.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "8.0,54.5,13.0,58.0"

[plantasjen.datasets.denmark.pbf.indexed]
file = "denmark-with-indexdata.osm.pbf"
xxhash = "a1b2c3d4e5f6..."
seq = 4704

[plantasjen.datasets.denmark.pbf.raw]
file = "denmark-raw.osm.pbf"
seq = 4704

[plantasjen.datasets.denmark.osc.4705]
file = "denmark-4705.osc.gz"
xxhash = "f1e2d3c4b5a6..."

# Optional historical snapshots - additional point-in-time captures of the
# same dataset, registered by hand or by pbfhogg's `pbfhogg-datasets` tool.
# The legacy top-level pbf/osc data above is implicitly snapshot `base`.
# `diff-snapshots --from base --to 20260411` diffs the two.
[plantasjen.datasets.denmark.snapshot.20260411]
download_date = "2026-04-11"
seq = 4969

[plantasjen.datasets.denmark.snapshot.20260411.pbf.raw]
file = "denmark-20260411.osm.pbf"
xxhash = "..."

[plantasjen.datasets.denmark.snapshot.20260411.pbf.indexed]
file = "denmark-20260411-with-indexdata.osm.pbf"
xxhash = "..."

```

- `project` - which supported project this config belongs to
- `[hostname.datasets.*]` - named datasets with PBF variants, OSC diffs, PMTiles entries, bounding box, and (pbfhogg) named GeoJSON extract polygons under `polygon.<name>`
- `[hostname.datasets.*.snapshot.<key>]` - additional historical snapshots of the dataset (different point-in-time PBFs of the same region). Each snapshot has its own `pbf` and optional `osc` tables. The legacy top-level data is implicitly snapshot `base` (a reserved name). Snapshot keys must match `[a-zA-Z0-9_-]+`. Snapshots are available to script benches in the resolved config. Download and promotion into snapshot blocks is pbfhogg's `pbfhogg-datasets` tool (`brokkr run --with pbfhogg pbfhogg-datasets -- download ...`), not a brokkr command.
- `xxhash` - optional XXH128 hash for file integrity checks (`sha256` accepted as alias during migration). Run `brokkr env` to see computed hashes for updating config
- `[hostname]` - per-host path overrides, port, drive configuration, and default cargo features; defaults to `data/`, `data/scratch/`, and cargo target dir
- `features` - host Cargo feature defaults, inherited according to the command's build path. See `docs/brokkr.toml.md` for the policy. CLI `--features` are additive where host features apply.
- `[[check]]` - top-level array of tables (not host-scoped). Each entry is one (clippy + test) sweep with the entry's feature flags. Fields: `name` (required, unique label - referenced by name from `[test.profiles.*].sweeps`), `features = [...]` (explicit list; the `features = "all"` sentinel is rejected so adding a new feature to `Cargo.toml` doesn't silently broaden coverage), `no_default_features = true|false`, `build_packages = ["pbfhogg-cli", ...]` (rebuilt with the entry's features before the test phase, keeping `tests/cli_*.rs` `CliInvoker` calls honest). Multiple entries catch lints that feature-gated proc-macro rewrites mask under any single feature set. The legacy `[check]` table form (with `consumer_features`) is rejected at parse time
- `[[lint_sweep]]` - top-level array of tables: a build shape `check` lints with clippy and rustdoc and never tests, for a shape whose tests cannot run here, such as a cross-target `target` the host cannot execute. Takes the build-shape keys of `[[check]]` (`name`, `features`, `no_default_features`, `packages`, `env`, `rustflags`, `profile`, `target`, `feature_unification`) plus `when = "off" | "gate" | "always"` (default `always`); test keys are load errors. See `docs/brokkr.toml.sweeps.md`
- `[[dependency_rule]]` - top-level array of direct Cargo dependency boundary rules enforced by `brokkr check`. Fields: optional `name`, `from = "app"` or `from = ["app", "rtsk"]`, and either `forbid = "db"` / `forbid = ["db", "rusqlite"]` or an allow-list `allow = ["libc"]` (exactly one)
- `[test]` - top-level (not host-scoped). `default_package = "..."` is the cargo package `brokkr test` uses when no `-p/--package` is given; required for multi-crate workspaces that lack a built-in default. Explicit CLI `-p` always wins. `default_profile = "..."` is the validation profile `brokkr check` uses when no `--profile` is passed - typically `tier1` for fast inner-loop runs. `[test.profiles.<name>]` declares a profile with `sweeps = [...]` (list of `[[check]]` entry names), optional `tests`/`only`/`skip`/`include_ignored`/`test_threads`/`env`, and optional `extends = "<other>"` (single-parent inheritance, child collections replace parent's). Profiles use Rust **module paths** as the annotation surface: tests inside `mod tier2 { ... }` / `mod platform { ... }` / `mod serial { ... }` are filtered by name substring (`only = ["tier2::"]`, `skip = ["platform::"]`). Dangling sweep references (profile names a `[[check]]` entry that doesn't exist) are caught at parse time. The legacy `[test.sweeps.*]` map form is rejected; sweeps live in `[[check]]` now

## License

Apache-2.0
